use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, BufReader};
use smol::Async;
use std::os::unix::net::UnixStream;

use super::agent_runner::{run_agent_turn, run_compaction, send};
use super::notifications::{
    auto_archive_done_sessions, broadcast_to_subscribers, broadcast_to_subscribers_and_wait,
    emit_phase, emit_phase_and_wait, is_no_agent_loop_session, last_assistant_text,
    notify_session_done_waiters, placeholder_no_agent_note, queue_and_maybe_resume,
    queue_info_to_session, queue_message_to_session, record_message_to_log_session,
};
use super::registry::{
    maybe_respawn_for_queued, model_info, session_info, session_info_from_db_stats,
    spawn_global_plugin_background_tasks,
};
use super::state::{SessionLocks, SharedState, lock_state, session_lock};
use super::{SharedTestOverrides, ShutdownHandle};
use crate::auth::AuthCredential;
use crate::compaction;
use crate::protocol::Response;
use crate::truncate_str;
use crate::types::*;

const USAGE_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Create a session (pure DB logic, no plugin setup).
#[allow(clippy::too_many_arguments)]
pub(super) fn create_session_impl(
    state: &SharedState,
    model_id: &Option<String>,
    provider_name: &Option<String>,
    system_prompt: &Option<String>,
    cwd: &Option<String>,
    parent_id: &Option<String>,
    child_budget: u32,
    tagline: &Option<String>,
    auto_archive: bool,
    notify_parent: bool,
    project_name: &Option<String>,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    let st = lock_state(state);

    // Budget check — flat cost of 1 per direct child (non-recursive).
    if let Some(pid) = parent_id {
        match st.db.get_session(pid) {
            Ok(Some(parent)) => {
                let used = st.db.child_count(&parent.id).unwrap_or(0) as u32;
                if used >= parent.child_budget {
                    return Response::Error {
                        message: format!(
                            "child budget exceeded: {} active children, budget is {}",
                            used, parent.child_budget
                        ),
                    };
                }
            }
            Ok(None) => {
                return Response::Error {
                    message: format!("parent session not found: {}", pid),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        }
    }

    // Load parent for inheritance
    let parent = parent_id
        .as_ref()
        .and_then(|pid| st.db.get_session(pid).ok().flatten());

    // Derive cwd before model resolution: project aliases need it.
    let requested_cwd = cwd.clone();
    let parent_cwd = parent.as_ref().and_then(|p| p.cwd.clone());
    let cwd = requested_cwd.clone().or_else(|| parent_cwd.clone());
    tracing::debug!(
        requested_cwd = ?requested_cwd,
        parent_cwd = ?parent_cwd,
        resolved_cwd = ?cwd,
        "create_session: cwd resolution",
    );

    // Resolve project name: explicit > discovery > parent inheritance.
    let explicit_project_name = project_name.clone();
    let discovery_result: Option<String> = cwd.as_deref().and_then(|c| {
        let path = std::path::Path::new(c);
        crate::project::discover_project(path).map(|(name, root)| {
            tracing::debug!(
                cwd = %c,
                name = %name,
                root = %root.display(),
                "create_session: discovery succeeded",
            );
            // Upsert: register the project in DB if not already present.
            let root_str = root.to_string_lossy();
            if st
                .db
                .get_project_by_path(&root_str)
                .ok()
                .flatten()
                .is_none()
            {
                let _ = st.db.create_project(&name, &root_str);
            } else {
                let _ = st.db.update_project_last_seen(&name);
            }
            name
        })
    });
    let parent_project = parent.as_ref().and_then(|p| p.project_name.clone());
    let project_name = explicit_project_name
        .clone()
        .or_else(|| discovery_result.clone())
        .or_else(|| parent_project.clone());
    tracing::debug!(
        explicit = ?explicit_project_name,
        discovery_result = ?discovery_result,
        parent_project = ?parent_project,
        final_resolved = ?project_name,
        "create_session: project_name resolution",
    );

    // Resolve the model. When the request supplies an explicit model_id we
    // run it through the alias resolver (operator → project → global →
    // literal id). When no model_id is given we inherit from the parent or
    // fall back to the server-wide default, preserving the historical
    // behavior for sessions that don't ask for a specific model.
    //
    // NOTE: load_project_aliases / load_operator_aliases perform file I/O
    // while we hold the state lock. This mirrors how
    // `load_project_instructions` is invoked from the task scheduler —
    // see models_config.rs for the rationale.
    //
    // Defence-in-depth (task #590): never inherit a model from a
    // "no-agent-loop" parent (e.g. the `log` placeholder) — doing so
    // bricks the new session because no provider will run an agent turn.
    // When the parent's provider reports `needs_api_key() == false`, we
    // treat the parent as if it had no model for inheritance purposes and
    // fall back to the server-wide default instead.
    let inheritable_parent_model = parent.as_ref().and_then(|p| {
        if st.registry.needs_api_key(&p.model.api) {
            Some(p.model.clone())
        } else {
            None
        }
    });
    let model = if let Some(mid) = model_id {
        let project_aliases = cwd
            .as_deref()
            .map(crate::models_config::load_project_aliases)
            .unwrap_or_default();
        // Discover project name from cwd for operator-tier aliases.
        let project_name = cwd.as_deref().and_then(|c| {
            tau_agent_base::project::discover_project(std::path::Path::new(c)).map(|(name, _)| name)
        });
        let operator_aliases = project_name
            .as_deref()
            .map(crate::models_config::load_operator_aliases)
            .unwrap_or_default();
        let merged_project =
            crate::models_config::merge_alias_maps(operator_aliases, project_aliases);
        let resolved = crate::model_resolve::resolve_model(
            mid,
            provider_name.as_deref(),
            Some(&merged_project).filter(|m| !m.is_empty()),
            &st.global_aliases,
            &st.all_models,
        );
        match resolved {
            Ok(m) => m.clone(),
            Err(e @ crate::model_resolve::ResolveError::UnknownAlias { .. }) => {
                // A configured alias points at a missing model: surface
                // the error instead of silently using the default.
                return Response::Error {
                    message: format!("{}. Use `tau models` to list available models.", e),
                };
            }
            Err(crate::model_resolve::ResolveError::UnknownModel { .. }) => {
                // Not an alias and not a known model id — preserve the
                // historical fall-through to parent / default.
                inheritable_parent_model
                    .clone()
                    .unwrap_or_else(|| st.default_model.clone())
            }
        }
    } else {
        inheritable_parent_model.unwrap_or_else(|| st.default_model.clone())
    };

    let id = match st.db.next_session_id() {
        Ok(id) => id,
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };

    let is_subscription = st
        .auth
        .get(&model.provider)
        .ok()
        .flatten()
        .is_some_and(|c| matches!(c, crate::auth::AuthCredential::Oauth(_)));

    let stored = crate::db::StoredSession {
        id: id.clone(),
        model,
        system_prompt: system_prompt.clone(),
        cwd,
        is_subscription,
        created_at: crate::types::timestamp_ms() as i64,
        parent_id: parent_id.clone(),
        child_budget,
        tagline: tagline.clone(),
        archived: false,
        last_exit_status: None,
        last_phase: None,
        auto_archive,
        notify_parent,
        project_name,
    };
    tracing::debug!(
        session_id = %id,
        project_name = ?stored.project_name,
        cwd = ?stored.cwd,
        "create_session: about to persist",
    );
    match st.db.create_session(&stored) {
        Ok(()) => Response::SessionCreated { session_id: id },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

pub(super) fn get_session_info_impl(
    state: &SharedState,
    session_id: &str,
) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.get_session(session_id) {
        Ok(Some(stored)) => {
            let messages = st.db.get_messages(session_id).unwrap_or_default();
            let last_msg = st.db.last_message_time(session_id).unwrap_or(None);
            let children = st.db.child_count(session_id).unwrap_or(0);
            crate::protocol::Response::SessionInfo {
                info: session_info(
                    &stored,
                    &messages,
                    last_msg,
                    children,
                    st.phases.get(session_id),
                    st.live_sessions.contains(session_id),
                ),
            }
        }
        Ok(None) => crate::protocol::Response::Error {
            message: format!("session not found: {}", session_id),
        },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

/// Maximum number of ancestors walked for `GetSessionAncestors`.  Prevents
/// a pathological cycle or malformed `parent_id` graph from hanging the
/// server.  The protocol docs promise leaf-first ordering up to this depth.
const ANCESTOR_DEPTH_GUARD: usize = 64;

pub(super) fn get_session_ancestors_impl(
    state: &SharedState,
    session_id: &str,
) -> crate::protocol::Response {
    let st = lock_state(state);
    let mut out = Vec::new();
    let mut current = session_id.to_string();
    for _ in 0..ANCESTOR_DEPTH_GUARD {
        let stored = match st.db.get_session(&current) {
            Ok(Some(s)) => s,
            // Unknown / stale id — stop and return what we have.  For the
            // very first lookup this produces an empty Vec, which is the
            // documented shape for an unknown `session_id`.
            Ok(None) => break,
            Err(e) => {
                return crate::protocol::Response::Error {
                    message: e.to_string(),
                };
            }
        };
        let messages = st.db.get_messages(&current).unwrap_or_default();
        let last_msg = st.db.last_message_time(&current).unwrap_or(None);
        let children = st.db.child_count(&current).unwrap_or(0);
        let info = session_info(
            &stored,
            &messages,
            last_msg,
            children,
            st.phases.get(&current),
            st.live_sessions.contains(&current),
        );
        let next_parent = info.parent_id.clone();
        out.push(info);
        match next_parent {
            Some(parent) => current = parent,
            None => break,
        }
    }
    crate::protocol::Response::SessionAncestors { sessions: out }
}

pub(super) fn get_messages_impl(
    state: &SharedState,
    session_id: &str,
) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.get_messages(session_id) {
        Ok(messages) => crate::protocol::Response::Messages { messages },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

/// Project-wide aggregate stats: totals across every session (archived
/// included) belonging to the named project.  See
/// [`crate::db::Db::project_stats`] for the underlying query.
pub(super) fn project_stats_impl(
    state: &SharedState,
    project_name: &str,
) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.project_stats(project_name) {
        Ok(s) => crate::protocol::Response::ProjectStats {
            stats: crate::protocol::ProjectStatsInfo {
                project_name: project_name.to_string(),
                session_count: s.session_count,
                message_count: s.message_count,
                tokens_input: s.tokens_input,
                tokens_output: s.tokens_output,
                tokens_cache_read: s.tokens_cache_read,
                tokens_cache_write: s.tokens_cache_write,
                cost_usd: s.cost,
                last_activity: s.last_message_time,
            },
        },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

/// Look up a project's metadata by name. Returns
/// [`Response::ProjectInfo`] with `project = None` when the project
/// doesn't exist (so callers don't have to distinguish "missing" from
/// "DB error" in the happy path). DB-level failures still surface as
/// [`Response::Error`].
pub(super) fn get_project_info_impl(
    state: &SharedState,
    project_name: &str,
) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.get_project(project_name) {
        Ok(Some(p)) => crate::protocol::Response::ProjectInfo {
            project: Some(crate::protocol::ProjectInfoEntry {
                name: p.name,
                path: p.path,
            }),
        },
        Ok(None) => crate::protocol::Response::ProjectInfo { project: None },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

pub(super) fn list_sessions_impl(
    state: &SharedState,
    include_archived: bool,
    project_name: Option<&str>,
) -> crate::protocol::Response {
    let st = lock_state(state);
    let sessions_result = if let Some(pn) = project_name {
        st.db.list_sessions_by_project(pn, include_archived)
    } else {
        st.db.list_sessions(include_archived)
    };
    match sessions_result {
        Ok(stored) => {
            let mut infos = Vec::new();
            for s in &stored {
                let db_stats = st.db.session_stats(&s.id).unwrap_or(None);
                let children = st.db.child_count(&s.id).unwrap_or(0);
                infos.push(session_info_from_db_stats(
                    s,
                    db_stats.as_ref(),
                    children,
                    st.phases.get(&s.id),
                    st.live_sessions.contains(&s.id),
                ));
            }
            crate::protocol::Response::Sessions { sessions: infos }
        }
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

pub(super) fn cancel_chat_impl(
    state: &SharedState,
    session_id: &str,
    session_locks: &SessionLocks,
    caller_session_id: Option<&str>,
) -> crate::protocol::Response {
    let mut st = lock_state(state);
    if let Some(flag) = st.cancel_flags.get(session_id) {
        flag.store(true, Ordering::Relaxed);
    } else {
        st.cancel_flags
            .insert(session_id.to_string(), Arc::new(AtomicBool::new(true)));
    }
    drop(st);

    // If no chat loop is running for this session, no one will read the cancel
    // flag. Immediately emit Cancelled + Phase(Idle) so the TUI never gets
    // stuck, and clear the stale flag.
    let lock = session_lock(session_locks, session_id);
    let was_idle = lock.try_lock().is_some();

    // Persist the info message *before* broadcasting Cancelled so
    // subscribers that re-fetch history on the event see the info line
    // already present.
    let info_text = match (caller_session_id, was_idle) {
        (Some(caller), true) => format!("Session cancelled by {caller}. (was idle)"),
        (Some(caller), false) => format!("Session cancelled by {caller}."),
        (None, true) => "Session cancelled. (was idle)".to_string(),
        (None, false) => "Session cancelled.".to_string(),
    };
    queue_info_to_session(state, session_id, &info_text);

    if was_idle {
        broadcast_to_subscribers(state, session_id, &crate::protocol::Response::Cancelled);
        emit_phase(state, session_id, crate::types::AgentPhase::Idle);
        let mut st = lock_state(state);
        st.cancel_flags.remove(session_id);
    }

    crate::protocol::Response::Ok
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client(
    stream: Async<UnixStream>,
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    test_overrides: SharedTestOverrides,
    bg_chat_spawn_tx: smol::channel::Sender<super::state::ChatSpawn>,
) -> crate::Result<()> {
    // Register for shutdown notifications
    let shutdown_rx = shutdown.register_client();
    let reader = BufReader::new(&stream);
    let mut writer = &stream;
    let mut lines = reader.lines();

    'outer: loop {
        // Wait for either a request line or a shutdown notification
        let line = {
            let line_fut = lines.next();
            let shutdown_fut = shutdown_rx.recv();

            match futures::future::select(std::pin::pin!(line_fut), std::pin::pin!(shutdown_fut))
                .await
            {
                futures::future::Either::Left((Some(line), _)) => {
                    line.map_err(|e: std::io::Error| crate::Error::Io(e.to_string()))?
                }
                futures::future::Either::Left((None, _)) => break, // client disconnected
                futures::future::Either::Right((Ok(msg), _)) => {
                    // Shutdown notification — send to client and exit
                    send(&mut writer, &msg).await.ok();
                    break;
                }
                futures::future::Either::Right((Err(_), _)) => break,
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: crate::protocol::Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(
                    &mut writer,
                    &Response::Error {
                        message: format!("bad request: {}", e),
                    },
                )
                .await?;
                continue;
            }
        };

        match req {
            crate::protocol::Request::CreateSession {
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
                sandbox_profile,
            } => {
                // Atomic budget check + session creation (single lock hold)
                let resp = create_session_impl(
                    &state,
                    &model_id,
                    &provider_name,
                    &system_prompt,
                    &cwd,
                    &parent_id,
                    child_budget,
                    &tagline,
                    auto_archive,
                    notify_parent,
                    &project_name,
                );

                // If created and no explicit system prompt, set up plugins
                // and update the prompt post-creation.
                if let Response::SessionCreated { ref session_id } = resp
                    && system_prompt.is_none()
                {
                    let id = session_id.clone();
                    let (cwd_resolved, project_resolved) = {
                        let st = lock_state(&state);
                        let stored = st.db.get_session(&id).ok().flatten();
                        (
                            stored.as_ref().and_then(|s| s.cwd.clone()),
                            stored.as_ref().and_then(|s| s.project_name.clone()),
                        )
                    };
                    let cwd_str = cwd_resolved.as_deref().unwrap_or("/tmp");
                    let mut pm = plugins.lock().expect("plugins mutex poisoned");
                    match pm.ensure_session_plugins(
                        &id,
                        cwd_str,
                        project_resolved.as_deref(),
                        sandbox_profile.as_deref(),
                    ) {
                        Ok(failures) => {
                            for msg in &failures {
                                queue_info_to_session(&state, &id, msg);
                            }
                        }
                        Err(e) => tracing::warn!(%e, "failed to spawn session plugins"),
                    }
                    let tool_prompts = pm.tool_prompts(&id, child_budget);
                    let prompt =
                        crate::system_prompt::build(&crate::system_prompt::PromptOptions {
                            cwd: cwd_resolved,
                            tools: tool_prompts,
                            ..Default::default()
                        });
                    let st = lock_state(&state);
                    if let Err(e) = st.db.update_system_prompt(&id, &prompt) {
                        tracing::warn!(%e, "failed to update system prompt");
                    }
                }

                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::GetSessionInfo { session_id } => {
                let resp = get_session_info_impl(&state, &session_id);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::GetSessionAncestors { session_id } => {
                let resp = get_session_ancestors_impl(&state, &session_id);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::Chat {
                session_id,
                text,
                attachments,
            } => {
                if shutdown.is_shutting_down() {
                    // Emit the terminal pair (Error, AgentDone) even on
                    // shutdown so any subscribed TUI transitions out of
                    // Streaming mode and back to Input. Skipping AgentDone
                    // here used to leave the TUI's state machine waiting
                    // indefinitely for a turn that will never complete.
                    let resp = Response::Error {
                        message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                    };
                    broadcast_to_subscribers(&state, &session_id, &resp);
                    let done_resp = Response::AgentDone;
                    broadcast_to_subscribers_and_wait(&state, &session_id, &done_resp).await;
                    send(&mut writer, &resp).await.ok();
                    send(&mut writer, &done_resp).await.ok();
                    continue;
                }

                // Short-circuit: Chat on a placeholder (log-provider)
                // session records the message but does NOT spin up the
                // agent loop. Emit an Info stream event with the note,
                // then AgentDone so the streaming client terminates
                // cleanly. See task 582.
                if is_no_agent_loop_session(&state, &session_id) {
                    record_message_to_log_session(&state, &session_id, &text);
                    let note = placeholder_no_agent_note(&session_id);
                    let info_resp = Response::Stream {
                        event: Box::new(StreamEvent::Status {
                            message: note.clone(),
                        }),
                    };
                    broadcast_to_subscribers(&state, &session_id, &info_resp);
                    send(&mut writer, &info_resp).await.ok();
                    let done_resp = Response::AgentDone;
                    broadcast_to_subscribers_and_wait(&state, &session_id, &done_resp).await;
                    send(&mut writer, &done_resp).await.ok();
                    continue;
                }

                // Acquire per-session lock — serializes concurrent Chat requests.
                // If another agent turn is running, this awaits until it finishes.
                // Try non-blocking lock first; if contended, notify and then block.
                let session_mutex = session_lock(&session_locks, &session_id);
                let _session_guard = match session_mutex.try_lock_arc() {
                    Some(guard) => guard,
                    None => {
                        emit_phase(&state, &session_id, crate::types::AgentPhase::Waiting);
                        session_mutex.lock_arc().await
                    }
                };

                // Reset (and create) the cancel flag for this session.
                let cancel_flag: Arc<AtomicBool> = {
                    let mut st = lock_state(&state);
                    let flag = st
                        .cancel_flags
                        .entry(session_id.clone())
                        .or_insert_with(|| Arc::new(AtomicBool::new(false)));
                    flag.store(false, Ordering::Relaxed);
                    flag.clone()
                };

                // Mark session as live (turn actively running).
                {
                    let mut st = lock_state(&state);
                    st.live_sessions.insert(session_id.clone());
                }

                emit_phase(&state, &session_id, crate::types::AgentPhase::Preparing);

                // Run the Chat handler body inside a closure so that any
                // error is caught and we *always* broadcast a terminal
                // response (AgentDone / Cancelled / Error) to subscribers.
                // Without this guarantee the TUI gets stuck in Streaming
                // mode forever when an internal error (e.g. DB write)
                // causes the handler to bail out early via `?`.
                let chat_result: Result<(bool, bool), crate::Error> = async {
                    // Load session
                    let session_data = {
                        let st = lock_state(&state);
                        match st.db.get_session(&session_id) {
                            Ok(Some(stored)) => {
                                let messages = st.db.get_messages(&session_id)?;
                                let cwd = stored.cwd.clone().unwrap_or_else(|| "/tmp".to_string());
                                Ok((stored, messages, cwd))
                            }
                            Ok(None) => Err(crate::Error::Io(format!(
                                "session not found: {}",
                                session_id
                            ))),
                            Err(e) => Err(e),
                        }
                    };
                    let (stored, mut messages, cwd) = session_data?;
                    let model = stored.model.clone();

                    // Ensure session plugins are spawned and notify session start
                    {
                        let mut pm = plugins.lock().expect("plugins mutex poisoned");
                        match pm.ensure_session_plugins(
                            &session_id,
                            &cwd,
                            stored.project_name.as_deref(),
                            None,
                        ) {
                            Ok(failures) => {
                                for msg in &failures {
                                    queue_info_to_session(&state, &session_id, msg);
                                }
                            }
                            Err(e) => tracing::warn!(%e, "failed to spawn session plugins"),
                        }
                        pm.notify_session_start_once(
                            &cwd,
                            &session_id,
                            stored.project_name.as_deref(),
                        );
                    }

                    // Repair any corrupted message history (e.g. daemon killed
                    // mid-tool-execution, leaving tool_use without tool_result).
                    let repair_stubs = crate::agent::repair_messages(&messages);
                    if !repair_stubs.is_empty() {
                        tracing::warn!(
                            session_id = %session_id,
                            stubs = repair_stubs.len(),
                            "repaired missing tool_result messages"
                        );
                        let st = lock_state(&state);
                        for stub in &repair_stubs {
                            if let Err(e) = st.db.append_message(&session_id, stub) {
                                tracing::warn!(%e, "db error persisting repair stub");
                            }
                        }
                        messages.extend(repair_stubs);
                    }

                    // If session was interrupted mid-tool-call, continue first
                    if crate::agent::needs_continuation(&messages) {
                        let mut context = Context {
                            system_prompt: stored.system_prompt.clone(),
                            messages: messages.clone(),
                            tools: Vec::new(),
                        };
                        let cont_result = run_agent_turn(
                            &state,
                            &plugins,
                            &shutdown,
                            cancel_flag.clone(),
                            &model,
                            &mut context,
                            &cwd,
                            &session_id,
                            &mut writer,
                            &throttle,
                            &session_locks,
                            &test_overrides,
                        )
                        .await;
                        match cont_result {
                            Ok(_agent_result) => {
                                // Messages already persisted incrementally via on_message
                                let st = lock_state(&state);
                                messages = st.db.get_messages(&session_id)?;
                            }
                            Err(e) => {
                                tracing::warn!(%e, "continuation error");
                            }
                        }
                    }

                    // Call before_agent_start hooks (plugins inject context)
                    let mut system_prompt = stored.system_prompt.clone();
                    {
                        let mut pm = plugins.lock().expect("plugins mutex poisoned");
                        let hook_data = serde_json::json!({
                            "prompt": &text,
                            "system_prompt": &system_prompt,
                            "session_id": &session_id,
                            "message_count": messages.len(),
                        });
                        let results = pm.call_hook(&session_id, "before_agent_start", &hook_data);
                        for result in results {
                            if let Some(msg) = result.message {
                                let ctx_msg = Message::User(UserMessage::text(&msg.content));
                                {
                                    let st = lock_state(&state);
                                    if let Err(e) = st.db.append_message(&session_id, &ctx_msg) {
                                        tracing::warn!(%e, "db error persisting hook context");
                                    }
                                }
                                messages.push(ctx_msg);
                            }
                            if let Some(sp) = result.system_prompt {
                                system_prompt = Some(sp);
                            }
                        }
                    }

                    // Append user message (persisted to DB).
                    // Build the engine-side message using the shared helper
                    // so attachments are validated and image blocks are
                    // appended after the text block (or alone if text is
                    // empty). Validation failures bail out cleanly via
                    // the surrounding async block's error path so the
                    // terminal-broadcast logic still fires.
                    let user_msg = match super::chat_attachments::build_user_message_for_request(
                        &text,
                        &attachments,
                    ) {
                        Ok(m) => m,
                        Err(e) => {
                            return Err(crate::Error::Io(format!(
                                "invalid chat attachments: {}",
                                e
                            )));
                        }
                    };
                    {
                        let st = lock_state(&state);
                        st.db.append_message(&session_id, &user_msg)?;
                        // Auto-derive tagline from first user message if not set
                        if stored.tagline.is_none() {
                            let tagline = text.replace('\n', " ");
                            let tagline = if tagline.len() > 80 {
                                format!("{}...", truncate_str(&tagline, 77))
                            } else {
                                tagline
                            };
                            let _ = st.db.update_tagline(&session_id, &tagline);
                        }
                    }
                    messages.push(user_msg);

                    // Broadcast user message to subscribers
                    broadcast_to_subscribers(
                        &state,
                        &session_id,
                        &Response::UserMessage { text: text.clone() },
                    );

                    let mut context = Context {
                        system_prompt,
                        messages,
                        tools: Vec::new(),
                    };
                    let result = run_agent_turn(
                        &state,
                        &plugins,
                        &shutdown,
                        cancel_flag.clone(),
                        &model,
                        &mut context,
                        &cwd,
                        &session_id,
                        &mut writer,
                        &throttle,
                        &session_locks,
                        &test_overrides,
                    )
                    .await;

                    let max_turns_reached = match result {
                        Ok(ref agent_result) => {
                            // Messages already persisted incrementally via on_message
                            agent_result.max_turns_reached
                        }
                        Err(crate::Error::Cancelled) => {
                            cancel_flag.store(true, Ordering::Relaxed);
                            false
                        }
                        Err(e) => {
                            // Update throttle on rate limit errors
                            if let crate::Error::Http(ref msg) = e {
                                throttle.handle_error(&model.provider, msg, None);
                            }
                            let err_msg = format!("agent error: {}", e);
                            throttle.handle_error(&model.provider, &err_msg, None);
                            return Err(e);
                        }
                    };

                    // Check compaction
                    let was_cancelled = cancel_flag.load(Ordering::Relaxed);
                    if !was_cancelled {
                        let should = {
                            let st = lock_state(&state);
                            let messages = st.db.get_messages(&session_id).unwrap_or_default();
                            let ctx_tokens = compaction::estimate_context_tokens(&messages);
                            compaction::should_compact(
                                ctx_tokens,
                                model.context_window,
                                &compaction::CompactionSettings::default(),
                            )
                        };
                        if should
                            && let Err(e) =
                                run_compaction(&state, &session_id, &model, None, false).await
                        {
                            tracing::warn!(%e, "compaction error");
                        }
                    }

                    Ok((was_cancelled, max_turns_reached))
                }
                .await;

                // Always broadcast a terminal response so subscribers
                // (especially the TUI) never get stuck in Streaming mode.
                // Terminal broadcasts (Cancelled, AgentDone, final Phase(Idle))
                // use the awaiting variant so the TUI observes them before the
                // session can appear idle via another code path. See the
                // module-level comment in notifications.rs.
                match chat_result {
                    Ok((true, _)) => {
                        // Cancelled
                        {
                            let st = lock_state(&state);
                            let _ = st.db.update_exit_status(&session_id, "cancelled");
                        }
                        let resp = Response::Cancelled;
                        broadcast_to_subscribers_and_wait(&state, &session_id, &resp).await;
                        send(&mut writer, &resp).await.ok();
                    }
                    Ok((false, max_turns_reached)) => {
                        // Normal completion (or max turns reached)
                        {
                            let st = lock_state(&state);
                            let status = if max_turns_reached {
                                "max_turns"
                            } else {
                                "completed"
                            };
                            let _ = st.db.update_exit_status(&session_id, status);
                        }
                        if max_turns_reached {
                            let status_resp = Response::Stream {
                                event: Box::new(StreamEvent::Status {
                                    message: "Reached tool use limit. Send a message to continue."
                                        .to_string(),
                                }),
                            };
                            // Status is best-effort but must precede AgentDone.
                            broadcast_to_subscribers(&state, &session_id, &status_resp);
                            send(&mut writer, &status_resp).await.ok();
                        }
                        let resp = Response::AgentDone;
                        broadcast_to_subscribers_and_wait(&state, &session_id, &resp).await;
                        send(&mut writer, &resp).await.ok();
                    }
                    Err(e) => {
                        {
                            let st = lock_state(&state);
                            let _ = st.db.update_exit_status(&session_id, "error");
                        }
                        let err_resp = Response::Error {
                            message: format!("agent error: {}", e),
                        };
                        let done_resp = Response::AgentDone;
                        broadcast_to_subscribers(&state, &session_id, &err_resp);
                        broadcast_to_subscribers_and_wait(&state, &session_id, &done_resp).await;
                        send(&mut writer, &err_resp).await.ok();
                        send(&mut writer, &done_resp).await.ok();
                    }
                }

                emit_phase_and_wait(&state, &session_id, crate::types::AgentPhase::Idle).await;
                // Mark session as no longer live.
                {
                    let mut st = lock_state(&state);
                    st.live_sessions.remove(&session_id);
                }
                notify_session_done_waiters(&state);

                // Before the session lock drops, check whether new messages
                // arrived during post-turn cleanup.  See doc on the function.
                maybe_respawn_for_queued(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    &test_overrides,
                );

                // Tier-3 post-idle drain: release the session lock, then run
                // queued post-idle actions (e.g. archive caller's subtree).
                // Actions must run AFTER the lock drops so they can freely
                // grab session locks without deadlocking.
                drop(_session_guard);
                super::post_idle::drain(&state, &session_id).await;

                if shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::ServerShutdown {
                            restart: shutdown.restart.load(Ordering::Relaxed),
                        },
                    )
                    .await
                    .ok();
                }
            }
            crate::protocol::Request::Subscribe { session_id } => {
                // Register this client as a subscriber for the session.
                // The connection stays open — we forward events via the channel.
                // No ack is sent; the client waits for Stream/AgentDone/Cancelled.
                let (tx, rx) = smol::channel::unbounded::<Response>();
                let tx_for_cleanup = tx.clone();
                {
                    let mut st = lock_state(&state);
                    st.subscribers
                        .entry(session_id.clone())
                        .or_default()
                        .push(tx);
                }

                // Send current agent phase so newly connected TUI shows correct state.
                // Include the server-stamped `turn_started_at_ms` and
                // `phase_started_at_ms` anchors so that subscribing to a
                // mid-flight session renders the "Working... Xs" counters
                // from the actual start of the turn / phase, not from the
                // moment the subscription was established.
                let phase_resp = {
                    let st = lock_state(&state);
                    let (phase, turn_ts, phase_ts) = st
                        .phases
                        .get(&session_id)
                        .copied()
                        .unwrap_or((crate::types::AgentPhase::default(), None, None));
                    Response::Stream {
                        event: Box::new(crate::types::StreamEvent::Phase {
                            phase,
                            turn_started_at_ms: turn_ts,
                            phase_started_at_ms: phase_ts,
                        }),
                    }
                };
                send(&mut writer, &phase_resp).await.ok();

                // Forward events until the channel closes, the client
                // disconnects, or the server shuts down.
                //
                // We three-way `select` over: channel recv, shutdown notice,
                // and the client's read half. The read half is included so
                // that a TUI dropping its socket (close / session-switch /
                // crash) wakes the loop immediately via EOF, rather than
                // pinning the subscriber slot until the next broadcast.
                // Subscribe consumes the connection, so we don't expect any
                // further bytes from the client; if some arrive (protocol
                // violation), we also exit and clean up.
                loop {
                    let recv_fut = rx.recv();
                    let shutdown_fut = shutdown_rx.recv();
                    let read_fut = lines.next();
                    let send_now = match futures::future::select(
                        std::pin::pin!(recv_fut),
                        futures::future::select(
                            std::pin::pin!(shutdown_fut),
                            std::pin::pin!(read_fut),
                        ),
                    )
                    .await
                    {
                        futures::future::Either::Left((Ok(resp), _)) => Some(resp),
                        futures::future::Either::Left((Err(_), _)) => None, // channel closed
                        futures::future::Either::Right((
                            futures::future::Either::Left((Ok(msg), _)),
                            _,
                        )) => {
                            // Shutdown — try to forward the message, then exit.
                            send(&mut writer, &msg).await.ok();
                            None
                        }
                        futures::future::Either::Right((
                            futures::future::Either::Left((Err(_), _)),
                            _,
                        )) => None,
                        futures::future::Either::Right((
                            futures::future::Either::Right((_read_outcome, _)),
                            _,
                        )) => {
                            // Read-half resolved: either EOF (client closed),
                            // an I/O error, or unexpected bytes after the
                            // Subscribe request. All three are exit signals.
                            None
                        }
                    };
                    match send_now {
                        Some(resp) => {
                            if send(&mut writer, &resp).await.is_err() {
                                break; // client disconnected mid-write
                            }
                        }
                        None => break,
                    }
                }

                // Always remove this handler's tx from the subscribers map
                // on loop exit, regardless of cause. Without this, a quiet
                // session whose only client disconnected would keep its
                // (dead) subscriber forever, blocking the idle sweep from
                // recycling the worker. Match by `same_channel` so we only
                // drop our own tx, not someone else's clone.
                {
                    let mut st = lock_state(&state);
                    if let Some(subs) = st.subscribers.get_mut(&session_id) {
                        subs.retain(|t| !t.same_channel(&tx_for_cleanup));
                        if subs.is_empty() {
                            st.subscribers.remove(&session_id);
                        }
                    }
                }
                break; // Subscribe consumes the connection
            }
            crate::protocol::Request::GetMessages { session_id } => {
                let resp = get_messages_impl(&state, &session_id);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::CancelChat {
                session_id,
                caller_session_id,
            } => {
                cancel_chat_impl(
                    &state,
                    &session_id,
                    &session_locks,
                    caller_session_id.as_deref(),
                );
                send(&mut writer, &Response::Ok).await.ok();
            }
            crate::protocol::Request::Steer { session_id, text } => {
                // Queue the message persistently; if the session is idle,
                // spawn a resume so it gets processed immediately.
                queue_and_maybe_resume(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    &text,
                    "steer",
                    &test_overrides,
                );
                send(&mut writer, &Response::Ok).await.ok();
            }
            crate::protocol::Request::Compact {
                session_id,
                keep_hint,
            } => {
                if shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                        },
                    )
                    .await
                    .ok();
                    continue;
                }

                // Reject if a turn is in flight: compaction mutates message
                // history and racing it against an active agent loop would
                // be confusing. The user can cancel the turn first.
                let is_live = {
                    let st = lock_state(&state);
                    st.live_sessions.contains(&session_id)
                };
                if is_live {
                    let info = Response::Stream {
                        event: Box::new(StreamEvent::Status {
                            message: "compaction rejected: a turn is currently running. \
                                 Cancel it first (Ctrl+C) and retry."
                                .to_string(),
                        }),
                    };
                    broadcast_to_subscribers(&state, &session_id, &info);
                    send(&mut writer, &info).await.ok();
                    send(&mut writer, &Response::Ok).await.ok();
                    continue;
                }

                // Look up the session's model. Compute the lookup result
                // synchronously so the DB lock is dropped before any await.
                enum ModelLookup {
                    Found(crate::types::Model),
                    NotFound,
                    DbErr(String),
                }
                let lookup = {
                    let st = lock_state(&state);
                    match st.db.get_session(&session_id) {
                        Ok(Some(stored)) => ModelLookup::Found(stored.model.clone()),
                        Ok(None) => ModelLookup::NotFound,
                        Err(e) => ModelLookup::DbErr(e.to_string()),
                    }
                };
                let model = match lookup {
                    ModelLookup::Found(m) => m,
                    ModelLookup::NotFound => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session not found: {}", session_id),
                            },
                        )
                        .await
                        .ok();
                        continue;
                    }
                    ModelLookup::DbErr(msg) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("db error: {}", msg),
                            },
                        )
                        .await
                        .ok();
                        continue;
                    }
                };

                // Acquire the per-session lock to serialize against any
                // other Chat / Compact request.
                let session_mutex = session_lock(&session_locks, &session_id);
                let _guard = match session_mutex.try_lock_arc() {
                    Some(guard) => guard,
                    None => {
                        emit_phase(&state, &session_id, crate::types::AgentPhase::Waiting);
                        session_mutex.lock_arc().await
                    }
                };

                // Mark live for the duration so subscribers see Compacting.
                {
                    let mut st = lock_state(&state);
                    st.live_sessions.insert(session_id.clone());
                }

                let res = run_compaction(
                    &state,
                    &session_id,
                    &model,
                    keep_hint.as_deref(),
                    true, // manual
                )
                .await;

                {
                    let mut st = lock_state(&state);
                    st.live_sessions.remove(&session_id);
                }
                emit_phase_and_wait(&state, &session_id, crate::types::AgentPhase::Idle).await;

                match res {
                    Ok(()) => {
                        // Outcome already broadcast by run_compaction and
                        // persisted as an Info message in the transcript;
                        // nothing more to send on the request connection.
                    }
                    Err(e) => {
                        // Surface the error to subscribers (the TUI's Subscribe
                        // connection) and persist a matching Info message so
                        // there's a durable record of the failure.
                        let err_text = format!("compaction error: {}", e);
                        let resp = Response::Stream {
                            event: Box::new(StreamEvent::Status {
                                message: err_text.clone(),
                            }),
                        };
                        broadcast_to_subscribers(&state, &session_id, &resp);
                        super::notifications::queue_info_to_session(&state, &session_id, &err_text);
                    }
                }
            }

            crate::protocol::Request::ListSessions {
                include_archived,
                project_name,
            } => {
                let resp = list_sessions_impl(&state, include_archived, project_name.as_deref());
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::ArchiveSession {
                session_id,
                require_ancestor,
            } => {
                // If require_ancestor is set, verify the target is a descendant
                if let Some(ref ancestor) = require_ancestor {
                    let is_desc = {
                        let st = lock_state(&state);
                        st.db.is_descendant(&session_id, ancestor)
                    };
                    match is_desc {
                        Ok(false) => {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!(
                                        "session {} is not a descendant of {}",
                                        session_id, ancestor
                                    ),
                                },
                            )
                            .await?;
                            continue;
                        }
                        Err(e) => {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: e.to_string(),
                                },
                            )
                            .await?;
                            continue;
                        }
                        Ok(true) => {} // proceed
                    }
                }

                // Validate: session must exist and all sessions in the subtree must be idle
                let subtree_ids = {
                    let st = lock_state(&state);
                    match st.db.get_session(&session_id)? {
                        Some(_) => Some(st.db.get_subtree_ids(&session_id)?),
                        None => None,
                    }
                };
                let subtree_ids = match subtree_ids {
                    Some(ids) => ids,
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session not found: {}", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                };

                // Check all sessions in the subtree are idle (not locked)
                let mut busy_id = None;
                for sid in &subtree_ids {
                    let lock = session_lock(&session_locks, sid);
                    if lock.try_lock().is_none() {
                        busy_id = Some(sid.clone());
                        break;
                    }
                }
                if let Some(busy) = busy_id {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: format!("cannot archive: session {} is busy", busy),
                        },
                    )
                    .await?;
                    continue;
                }

                // Send info messages before archiving
                for sid in &subtree_ids {
                    queue_info_to_session(&state, sid, "Session archived.");
                }

                // Archive in DB
                {
                    let st = lock_state(&state);
                    st.db.archive_session_tree(&session_id)?;
                }

                // Idle all session plugins for archived sessions
                {
                    let mut pm = plugins.lock().expect("plugins mutex poisoned");
                    for sid in &subtree_ids {
                        pm.destroy_session_plugins(sid);
                    }
                }

                // Clean up in-memory state for archived sessions
                {
                    let mut st = lock_state(&state);
                    for sid in &subtree_ids {
                        st.cancel_flags.remove(sid);
                        st.has_queued.remove(sid);
                        st.subscribers.remove(sid);
                        st.phases.remove(sid);
                        st.waited_sessions.remove(sid);
                    }
                }

                send(&mut writer, &Response::SessionArchived).await?;
            }
            crate::protocol::Request::RestoreSession { session_id } => {
                // Validate: session must exist and be archived
                let exists = {
                    let st = lock_state(&state);
                    st.db.get_session(&session_id)?
                };
                match exists {
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session not found: {}", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                    Some(ref s) if !s.archived => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session {} is not archived", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                    _ => {}
                }

                // Restore in DB
                {
                    let st = lock_state(&state);
                    st.db.restore_session_tree(&session_id)?;
                }

                // Send info message to all sessions in the subtree after restoring
                {
                    let subtree_ids = {
                        let st = lock_state(&state);
                        st.db.get_subtree_ids(&session_id)?
                    };
                    for sid in &subtree_ids {
                        queue_info_to_session(&state, sid, "Session restored.");
                    }
                }

                send(&mut writer, &Response::SessionRestored).await?;
            }
            crate::protocol::Request::DeleteSession { session_id } => {
                // Collect all session IDs in the subtree before deleting
                let subtree_ids = {
                    let st = lock_state(&state);
                    let ids = st.db.get_subtree_ids(&session_id)?;
                    // Delete session and all descendants
                    st.db.delete_session_tree(&session_id)?;
                    // Clean up waited_sessions for deleted IDs
                    ids
                };
                {
                    let mut st = lock_state(&state);
                    for id in &subtree_ids {
                        st.waited_sessions.remove(id);
                    }
                }
                // Clean up session plugins for all deleted sessions
                {
                    let mut pm = plugins.lock().expect("plugins mutex poisoned");
                    for id in &subtree_ids {
                        pm.destroy_session_plugins(id);
                    }
                }
                send(&mut writer, &Response::SessionDeleted).await?;
            }
            crate::protocol::Request::ListModels => {
                let models = {
                    let st = lock_state(&state);
                    st.all_models.iter().map(model_info).collect::<Vec<_>>()
                };
                send(&mut writer, &Response::Models { models }).await?;
            }
            crate::protocol::Request::ListAliases { cwd } => {
                use crate::protocol::AliasInfo;
                let global: Vec<AliasInfo> = {
                    let st = lock_state(&state);
                    let mut entries: Vec<AliasInfo> = st
                        .global_aliases
                        .iter()
                        .map(|(name, target)| AliasInfo {
                            name: name.clone(),
                            target: target.clone(),
                        })
                        .collect();
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    entries
                };
                let project: Vec<AliasInfo> = match cwd.as_deref() {
                    Some(c) => {
                        let project_map = crate::models_config::load_project_aliases(c);
                        // Discover project name for operator-tier aliases.
                        let proj_name =
                            tau_agent_base::project::discover_project(std::path::Path::new(c))
                                .map(|(name, _)| name);
                        let operator_map = proj_name
                            .as_deref()
                            .map(crate::models_config::load_operator_aliases)
                            .unwrap_or_default();
                        let merged =
                            crate::models_config::merge_alias_maps(operator_map, project_map);
                        let mut entries: Vec<AliasInfo> = merged
                            .into_iter()
                            .map(|(name, target)| AliasInfo { name, target })
                            .collect();
                        entries.sort_by(|a, b| a.name.cmp(&b.name));
                        entries
                    }
                    None => Vec::new(),
                };
                send(&mut writer, &Response::Aliases { global, project }).await?;
            }
            crate::protocol::Request::SetCwd {
                session_id,
                cwd,
                caller_session_id,
            } => {
                let result = {
                    let st = lock_state(&state);
                    st.db.update_cwd(&session_id, &cwd)
                };
                match result {
                    Ok(()) => {
                        let info = match caller_session_id.as_deref() {
                            Some(caller) => format!("cwd changed by {caller} to {cwd}."),
                            None => format!("cwd changed to {cwd}."),
                        };
                        queue_info_to_session(&state, &session_id, &info);
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::ReparentChildren {
                old_parent_id,
                new_parent_id,
            } => {
                // Capture the affected children *before* the DB update so we
                // know which sessions to annotate (the reparent query changes
                // the parent pointer in-place, so after it runs a lookup by
                // `old_parent_id` would return nothing).
                let children: Vec<String> = {
                    let st = lock_state(&state);
                    st.db
                        .get_children(&old_parent_id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| s.id)
                        .collect()
                };
                let result = {
                    let st = lock_state(&state);
                    st.db.reparent_children(&old_parent_id, &new_parent_id)
                };
                match result {
                    Ok(()) => {
                        let info = format!(
                            "Parent session changed from {old_parent_id} to {new_parent_id}."
                        );
                        for child_id in &children {
                            queue_info_to_session(&state, child_id, &info);
                        }
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::SetModel {
                session_id,
                model_id,
                caller_session_id,
            } => {
                let result = {
                    let st = lock_state(&state);
                    // Look up the session's cwd so project aliases (loaded
                    // from `{cwd}/.tau/models.toml`) apply when the user
                    // switches models mid-session via `/model smart`.
                    let cwd = st
                        .db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.cwd);
                    let project_aliases = cwd
                        .as_deref()
                        .map(crate::models_config::load_project_aliases)
                        .unwrap_or_default();
                    // Discover project name for operator-tier aliases.
                    let proj_name = cwd.as_deref().and_then(|c| {
                        tau_agent_base::project::discover_project(std::path::Path::new(c))
                            .map(|(name, _)| name)
                    });
                    let operator_aliases = proj_name
                        .as_deref()
                        .map(crate::models_config::load_operator_aliases)
                        .unwrap_or_default();
                    let merged_project =
                        crate::models_config::merge_alias_maps(operator_aliases, project_aliases);
                    match crate::model_resolve::resolve_model(
                        &model_id,
                        None,
                        Some(&merged_project).filter(|m| !m.is_empty()),
                        &st.global_aliases,
                        &st.all_models,
                    ) {
                        Ok(model) => {
                            st.db.update_model(&session_id, model)?;
                            Ok(model_info(model))
                        }
                        Err(e) => Err(format!("{}. Use /model to list available models.", e)),
                    }
                };
                match result {
                    Ok(info) => {
                        let info_text = match caller_session_id.as_deref() {
                            Some(caller) => format!(
                                "Model changed by {caller} to {} ({}).",
                                info.name, info.provider
                            ),
                            None => format!("Model changed to {} ({}).", info.name, info.provider),
                        };
                        queue_info_to_session(&state, &session_id, &info_text);
                        send(&mut writer, &Response::ModelChanged { model: info }).await?;
                    }
                    Err(msg) => {
                        send(&mut writer, &Response::Error { message: msg }).await?;
                    }
                }
            }
            crate::protocol::Request::Login { provider } => {
                let result = smol::unblock(move || {
                    if provider == "anthropic" {
                        crate::auth::login_anthropic()
                    } else {
                        Err(crate::Error::Io(format!(
                            "unknown OAuth provider: {}",
                            provider
                        )))
                    }
                })
                .await;

                match result {
                    Ok(creds) => {
                        let provider_name = "anthropic".to_string();
                        let save_result = {
                            let st = lock_state(&state);
                            st.auth.set(&provider_name, AuthCredential::Oauth(creds))
                        };
                        if let Err(e) = save_result {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!("failed to save credentials: {}", e),
                                },
                            )
                            .await?;
                        } else {
                            send(
                                &mut writer,
                                &Response::LoginSuccess {
                                    provider: provider_name,
                                },
                            )
                            .await?;
                        }
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("login failed: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::AuthStatus => {
                let providers = {
                    let st = lock_state(&state);
                    st.auth.list().unwrap_or_default()
                };
                send(&mut writer, &Response::AuthStatus { providers }).await?;
            }
            crate::protocol::Request::GetSubscriptionUsage => {
                // Check cache, fetch if stale
                let cache_result = {
                    let st = lock_state(&state);
                    let now = crate::types::timestamp_ms();
                    if let Some((ref usage, fetched_at)) = st.usage_cache {
                        if now.saturating_sub(fetched_at) < USAGE_CACHE_TTL_MS {
                            Some(Ok(usage.clone()))
                        } else {
                            None // stale
                        }
                    } else {
                        None // not yet fetched
                    }
                };

                let result = if let Some(cached) = cache_result {
                    cached
                } else {
                    // Fetch outside the lock
                    let token = {
                        let st = lock_state(&state);
                        st.auth.get_api_key("anthropic")
                    };
                    match token {
                        Ok(Some(tok)) if crate::auth::is_oauth_token(&tok) => {
                            smol::unblock(move || crate::auth::fetch_subscription_usage(&tok)).await
                        }
                        _ => Err(crate::Error::NoApiKey(
                            "subscription usage requires OAuth login".into(),
                        )),
                    }
                };

                match result {
                    Ok(usage) => {
                        // Update cache
                        {
                            let mut st = lock_state(&state);
                            st.usage_cache = Some((usage.clone(), crate::types::timestamp_ms()));
                        }
                        send(&mut writer, &Response::SubscriptionUsage { usage }).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs,
            } => {
                if shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                        },
                    )
                    .await
                    .ok();
                    continue;
                }
                // Wait for all specified sessions to have no active agent turn.
                // A session is "done" if it's not currently locked (no active Chat).
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let mut results = Vec::new();

                // Register a waiter channel to be notified on session completion.
                let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.insert(sid.clone());
                    }
                    st.session_done_waiters.push(notify_tx);
                }

                loop {
                    // Short-circuit on shutdown so the waiting client
                    // gets a distinctive signal instead of blocking until
                    // deadline while the server is draining.
                    if shutdown.is_shutting_down() {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                            },
                        )
                        .await
                        .ok();
                        // Clean up waited_sessions bookkeeping before bailing.
                        let mut st = lock_state(&state);
                        for sid in &session_ids {
                            st.waited_sessions.remove(sid);
                        }
                        continue 'outer;
                    }
                    let mut all_done = true;
                    results.clear();

                    for sid in &session_ids {
                        // Check if session has an active agent turn by trying the lock
                        let lock = session_lock(&session_locks, sid);
                        let is_busy = lock.try_lock().is_none();

                        if is_busy {
                            all_done = false;
                            results.push(crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status: "busy".into(),
                                summary: String::new(),
                            });
                        } else {
                            // Session is idle -- get its last assistant message as summary
                            let st = lock_state(&state);
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
                        // Mark timed-out sessions
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
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.remove(sid);
                    }
                }
                // Close receiver so the sender is pruned on next notify.
                drop(notify_rx);

                auto_archive_done_sessions(&state, &results);
                send(&mut writer, &Response::SessionsCompleted { results }).await?;
            }
            crate::protocol::Request::WaitAnySessions {
                session_ids,
                timeout_secs,
            } => {
                if shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                        },
                    )
                    .await
                    .ok();
                    continue;
                }
                // Wait until at least one session completes.
                // Returns results for all sessions that are done at that point.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let results;

                // Register a waiter channel to be notified on session completion.
                let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.insert(sid.clone());
                    }
                    st.session_done_waiters.push(notify_tx);
                }

                loop {
                    if shutdown.is_shutting_down() {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                            },
                        )
                        .await
                        .ok();
                        let mut st = lock_state(&state);
                        for sid in &session_ids {
                            st.waited_sessions.remove(sid);
                        }
                        continue 'outer;
                    }
                    let mut done = Vec::new();

                    for sid in &session_ids {
                        let lock = session_lock(&session_locks, sid);
                        let is_busy = lock.try_lock().is_none();

                        if !is_busy {
                            let st = lock_state(&state);
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
                            // Timeout -- mark all as timeout
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
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.remove(sid);
                    }
                }
                drop(notify_rx);

                auto_archive_done_sessions(&state, &results);
                send(&mut writer, &Response::SessionsCompleted { results }).await?;
            }
            crate::protocol::Request::QueueMessage {
                target_session_id,
                content,
                sender_info,
                await_reply,
                reply_to: _,
            } => {
                if await_reply && shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                        },
                    )
                    .await
                    .ok();
                    continue;
                }

                // Short-circuit: messages to a placeholder (log-provider)
                // session are recorded to history but do NOT trigger an
                // agent loop. Return a courtesy note so the caller knows
                // no reply will arrive. See task 582.
                if is_no_agent_loop_session(&state, &target_session_id) {
                    record_message_to_log_session(&state, &target_session_id, &content);
                    let note = placeholder_no_agent_note(&target_session_id);
                    let resp = if await_reply {
                        Response::MessageReply { content: note }
                    } else {
                        Response::OkWithNote { note }
                    };
                    send(&mut writer, &resp).await?;
                    continue;
                }

                if await_reply {
                    // Generate a unique msg_id, create a oneshot channel,
                    // prefix the message so the target knows to reply.
                    let (msg_id, rx) = {
                        let mut st = lock_state(&state);
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
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &target_session_id,
                        &prefixed,
                        &sender_info,
                        &test_overrides,
                    );

                    // Wait for reply with a timeout (default 5 min).
                    // Also periodically check for shutdown so the caller
                    // learns to reconnect rather than blocking through a
                    // restart.
                    let timeout = std::time::Duration::from_secs(300);
                    let deadline = std::time::Instant::now() + timeout;
                    let outcome: Option<Result<String, ()>> = loop {
                        if shutdown.is_shutting_down() {
                            break None;
                        }
                        let now = std::time::Instant::now();
                        if now >= deadline {
                            break Some(Err(()));
                        }
                        let step = (deadline - now).min(std::time::Duration::from_millis(200));
                        match futures::future::select(
                            std::pin::pin!(rx.recv()),
                            std::pin::pin!(smol::Timer::after(step)),
                        )
                        .await
                        {
                            futures::future::Either::Left((Ok(reply), _)) => {
                                break Some(Ok(reply));
                            }
                            futures::future::Either::Left((Err(_), _)) => {
                                // Sender was dropped — treat as cancellation.
                                break Some(Err(()));
                            }
                            futures::future::Either::Right((_, _)) => {
                                // Step elapsed; re-check shutdown flag.
                                continue;
                            }
                        }
                    };
                    match outcome {
                        Some(Ok(reply)) => {
                            send(&mut writer, &Response::MessageReply { content: reply }).await?;
                        }
                        None => {
                            // Shutdown: clean up waiter and tell client to reconnect.
                            {
                                let mut st = lock_state(&state);
                                st.reply_waiters.remove(&msg_id);
                            }
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: crate::protocol::SHUTTING_DOWN_ERROR.into(),
                                },
                            )
                            .await
                            .ok();
                        }
                        Some(Err(_)) => {
                            // Timeout or channel closed — clean up waiter.
                            {
                                let mut st = lock_state(&state);
                                st.reply_waiters.remove(&msg_id);
                            }
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!("await_reply timed out (msg_id={})", msg_id),
                                },
                            )
                            .await?;
                        }
                    }
                } else {
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &target_session_id,
                        &content,
                        &sender_info,
                        &test_overrides,
                    );
                    send(&mut writer, &Response::Ok).await?;
                }
            }
            crate::protocol::Request::QueueInfo {
                target_session_id,
                text,
            } => {
                queue_info_to_session(&state, &target_session_id, &text);
                send(&mut writer, &Response::Ok).await?;
            }
            crate::protocol::Request::ReplyToMessage { msg_id, content } => {
                let result = {
                    let mut st = lock_state(&state);
                    st.reply_waiters.remove(&msg_id)
                };
                match result {
                    Some(tx) => {
                        let _ = tx.send(content).await;
                        send(&mut writer, &Response::Ok).await?;
                    }
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("no pending waiter for msg_id={}", msg_id),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::ReloadPlugins { session_id } => {
                let (cwd, project_name) = {
                    let st = lock_state(&state);
                    match st.db.get_session(&session_id).ok().flatten() {
                        Some(s) => (s.cwd.unwrap_or_else(|| "/tmp".to_string()), s.project_name),
                        None => ("/tmp".to_string(), None),
                    }
                };
                let result = {
                    let mut pm = plugins.lock().expect("plugins mutex poisoned");
                    pm.reload_config();
                    pm.destroy_session_plugins(&session_id);
                    pm.ensure_session_plugins(&session_id, &cwd, project_name.as_deref(), None)
                        .map(|failures| {
                            // Emit the success/event line first so readers see
                            // "reload happened; here's what broke" in order.
                            queue_info_to_session(&state, &session_id, "Plugins reloaded.");
                            for msg in &failures {
                                queue_info_to_session(&state, &session_id, msg);
                            }
                            pm.load_global_plugins(&cwd)
                        })
                };
                match result {
                    Ok(()) => {
                        // Restart background tasks for the new global plugins.
                        spawn_global_plugin_background_tasks(
                            &plugins,
                            &state,
                            &session_locks,
                            &shutdown,
                            &throttle,
                            &bg_chat_spawn_tx,
                            &test_overrides,
                        );
                        queue_message_to_session(
                            &state,
                            &session_id,
                            "[System: plugins reloaded. Tool definitions updated.]",
                            "system",
                        );
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("reload session plugins: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::ReloadConfig => {
                // Do the disk I/O *outside* the state lock so we only
                // hold the mutex for the swap. `load_runtime_config` can
                // fail (providers.toml parse/IO error); on failure we
                // leave the running state untouched so a broken edit
                // can't brick a live server.
                match super::load_runtime_config() {
                    Ok(new_cfg) => {
                        {
                            let mut st = lock_state(&state);
                            let old_default = (
                                st.default_model.provider.clone(),
                                st.default_model.id.clone(),
                            );
                            st.config = new_cfg.config;
                            st.all_models = new_cfg.all_models;
                            st.global_aliases = new_cfg.global_aliases;
                            // Preserve the previously-selected default
                            // model's identity when possible; fall back
                            // to whatever `load_runtime_config` produced
                            // (which is `all_models[0]`).
                            st.default_model = st
                                .all_models
                                .iter()
                                .find(|m| m.provider == old_default.0 && m.id == old_default.1)
                                .cloned()
                                .unwrap_or(new_cfg.default_model);
                        }
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("reload config: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::GcSessions { older_than_days } => {
                let older_than_ms = {
                    let now = crate::types::timestamp_ms();
                    now.saturating_sub(older_than_days * 24 * 60 * 60 * 1000)
                };
                let result = {
                    let st = lock_state(&state);
                    st.db.gc_archived_sessions(older_than_ms)
                };
                match result {
                    Ok(deleted) => {
                        send(&mut writer, &Response::GcComplete { deleted }).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            crate::protocol::Request::ExecuteTool {
                session_id,
                tool_name,
                arguments,
            } => {
                let resp = super::tool_dispatch::execute_tool_impl(
                    &state,
                    &plugins,
                    &session_locks,
                    &shutdown,
                    &throttle,
                    &test_overrides,
                    &session_id,
                    &tool_name,
                    arguments,
                    &bg_chat_spawn_tx,
                )
                .await;
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::FireHook { .. } => {
                // FireHook is only valid from the plugin ServerRequest tunnel,
                // not from direct client connections.
                send(
                    &mut writer,
                    &Response::Error {
                        message: "fire_hook is only available from plugins".into(),
                    },
                )
                .await?;
            }
            crate::protocol::Request::Shutdown { restart } => {
                shutdown.request_shutdown(restart);
                send(&mut writer, &Response::Ok).await?;
                return Ok(());
            }
            crate::protocol::Request::EnqueuePostIdleAction { .. } => {
                // Post-idle enqueueing is only meaningful from a plugin
                // context where the caller's session is known. External
                // clients should not be able to enqueue post-idle work.
                send(
                    &mut writer,
                    &Response::Error {
                        message: "enqueue_post_idle_action is only available from plugins".into(),
                    },
                )
                .await?;
            }
            crate::protocol::Request::SetTagline {
                session_id,
                tagline,
            } => {
                let resp = {
                    let st = lock_state(&state);
                    match st.db.update_tagline(&session_id, &tagline) {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    }
                };
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskList {
                project,
                state: state_filter,
                parent_id,
            } => {
                let resp = super::task_handlers::handle_task_list(
                    &state,
                    &project,
                    state_filter.as_deref(),
                    parent_id,
                );
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskGet { id } => {
                let resp = super::task_handlers::handle_task_get(&state, id);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskCreate {
                project,
                title,
                parent_id,
                priority,
                tags,
                sandbox_profile,
            } => {
                let resp = super::task_handlers::handle_task_create(
                    &project,
                    &title,
                    parent_id,
                    priority,
                    &tags,
                    sandbox_profile.as_deref(),
                );
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskUpdate {
                id,
                state: new_state,
                title,
                priority,
                tags,
                affected_files,
                skip_review,
                require_approval,
                sandbox_profile,
            } => {
                let resp = super::task_handlers::handle_task_update(
                    id,
                    new_state,
                    title,
                    priority,
                    tags,
                    affected_files,
                    skip_review,
                    require_approval,
                    sandbox_profile,
                );
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskSearch {
                project,
                query,
                state: state_filter,
            } => {
                let resp = super::task_handlers::handle_task_search(
                    &project,
                    &query,
                    state_filter.as_deref(),
                );
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskAssign { id, session_id } => {
                let resp = super::task_handlers::handle_task_assign(id, &session_id);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskStatus { project } => {
                let resp = super::task_handlers::handle_task_status(&project);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskOverview {
                project,
                recent_limit,
            } => {
                let resp =
                    super::task_handlers::handle_task_overview(&state, &project, recent_limit);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::TaskMergeQueue { project } => {
                let resp = super::task_handlers::handle_task_merge_queue(&project);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::ProjectStats { project_name } => {
                let resp = project_stats_impl(&state, &project_name);
                send(&mut writer, &resp).await?;
            }
            crate::protocol::Request::GetProjectInfo { project_name } => {
                let resp = get_project_info_impl(&state, &project_name);
                send(&mut writer, &resp).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::provider::ProviderRegistry;
    use crate::server::state::State;
    use crate::types::{Model, ModelCost, ThinkingStyle};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn mk_model() -> Model {
        Model {
            id: "test/test".into(),
            name: "test".into(),
            api: "anthropic".into(),
            provider: "test".into(),
            base_url: "".into(),
            thinking: ThinkingStyle::default(),
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: HashMap::new(),
        }
    }

    fn mk_state() -> SharedState {
        let db = Db::open_memory().expect("open memory db");
        Arc::new(Mutex::new(State {
            db,
            registry: ProviderRegistry::new(),
            auth: crate::auth::AuthStorage::open_default(),
            config: crate::config::Config::default(),
            global_aliases: HashMap::new(),
            default_model: mk_model(),
            all_models: vec![mk_model()],
            usage_cache: None,
            cancel_flags: HashMap::new(),
            has_queued: HashMap::new(),
            subscribers: HashMap::new(),
            phases: HashMap::new(),
            live_sessions: HashSet::new(),
            waited_sessions: HashSet::new(),
            session_done_waiters: Vec::new(),
            reply_waiters: HashMap::new(),
            next_msg_id: 0,
            bg_after_idle: HashMap::new(),
            bg_scheduler: None,
        }))
    }

    /// Subscribing then dropping the client socket without any broadcast
    /// must clean up the subscriber entry within a bounded window. This
    /// is the disconnect-EOF race: previously the forwarding loop could
    /// only wake on `rx.recv()` or shutdown, leaving a dead `tx` clone
    /// pinned in `state.subscribers[sid]` indefinitely on a quiet
    /// session and blocking the idle sweep.
    #[test]
    fn subscriber_is_cleaned_up_on_client_disconnect() {
        smol::block_on(async {
            let state = mk_state();
            let plugins = Arc::new(Mutex::new(crate::plugin::PluginManager::new(
                crate::plugin::PluginsConfig {
                    no_default_worker: true,
                    ..Default::default()
                },
            )));
            let shutdown = ShutdownHandle::new();
            let session_locks: SessionLocks = Arc::new(Mutex::new(HashMap::new()));
            let throttle = crate::throttle::ProviderThrottle::new();
            let test_overrides: SharedTestOverrides =
                Arc::new(super::super::TestOverrides::default());
            let (bg_chat_tx, _bg_chat_rx) =
                smol::channel::unbounded::<crate::server::state::ChatSpawn>();

            let (client_std, server_std) =
                std::os::unix::net::UnixStream::pair().expect("socketpair");
            let server_async = Async::new(server_std).expect("async server stream");

            let sid = "s-disconnect-test".to_string();
            let state_for_handler = state.clone();
            let handler_task = smol::spawn(async move {
                let _ = handle_client(
                    server_async,
                    state_for_handler,
                    plugins,
                    shutdown,
                    session_locks,
                    throttle,
                    test_overrides,
                    bg_chat_tx,
                )
                .await;
            });

            // Send Subscribe request over the client end. We use a
            // smol::Async wrapper so writes interleave cleanly with the
            // server's task on the same executor.
            let client_async = Async::new(client_std).expect("async client stream");
            {
                use futures::AsyncWriteExt;
                let req = serde_json::to_string(&crate::protocol::Request::Subscribe {
                    session_id: sid.clone(),
                })
                .expect("serialize subscribe");
                let mut line = req;
                line.push('\n');
                let mut w = &client_async;
                w.write_all(line.as_bytes()).await.expect("write subscribe");
                w.flush().await.expect("flush");
            }

            // Wait for the subscriber to appear in state.subscribers.
            // The handler registers it before sending the initial Phase
            // event, so this should resolve within a few ms.
            let appeared = {
                let deadline = Instant::now() + Duration::from_millis(500);
                loop {
                    let count = lock_state(&state)
                        .subscribers
                        .get(&sid)
                        .map(|v| v.len())
                        .unwrap_or(0);
                    if count == 1 {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    smol::Timer::after(Duration::from_millis(5)).await;
                }
            };
            assert!(
                appeared,
                "subscriber was not registered in state.subscribers"
            );

            // Drop the client end of the socket without any broadcast
            // to the session. The server-side forwarding loop must
            // notice the EOF on its read half and exit, removing the
            // subscriber from the map.
            drop(client_async);

            // Within a bounded window, the entry must be gone.
            let cleaned = {
                let deadline = Instant::now() + Duration::from_millis(500);
                loop {
                    let still_present = lock_state(&state)
                        .subscribers
                        .get(&sid)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if !still_present {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    smol::Timer::after(Duration::from_millis(5)).await;
                }
            };
            assert!(
                cleaned,
                "subscriber was not cleaned up within 500ms of client disconnect"
            );

            // Best-effort: let the handler task finish cleanly.
            let _ = futures::future::select(
                handler_task,
                std::pin::pin!(smol::Timer::after(Duration::from_millis(200))),
            )
            .await;
        });
    }

    /// Symptom B regression (task #888): when `create_session_impl` is
    /// called with `project_name = None` and a `cwd` that points inside a
    /// tau project, the persisted `StoredSession.project_name` must be the
    /// discovered name. The dispatch loop then re-fetches that row and
    /// passes the resolved value to `ensure_session_plugins`, ensuring the
    /// plugin layer and the DB agree.
    #[test]
    fn create_session_resolves_project_name_from_cwd() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"resolved-proj\"\n").unwrap();

        let state = mk_state();
        let cwd = Some(root.to_string_lossy().into_owned());
        let resp = create_session_impl(
            &state,
            &None,
            &None,
            &Some("sp".into()), // bypass post-create plugin spawn path; we test resolution only
            &cwd,
            &None,
            0,
            &None,
            false,
            true,
            &None, // <-- explicit project_name is None: discovery must populate
        );
        let id = match resp {
            crate::protocol::Response::SessionCreated { session_id } => session_id,
            other => panic!("expected SessionCreated, got {:?}", other),
        };

        // The persisted row must carry the discovered project name.
        let stored = {
            let st = lock_state(&state);
            st.db
                .get_session(&id)
                .expect("db.get_session")
                .expect("session row")
        };
        assert_eq!(
            stored.project_name.as_deref(),
            Some("resolved-proj"),
            "discovery must populate project_name when explicit value is None",
        );

        // Mirror the dispatch loop's fetch-and-pass step: the value handed
        // to the plugin layer is `stored.project_name`, not the (None) wire
        // value that was originally requested. This is the contract the
        // fix in dispatch.rs upholds.
        let plugin_arg: Option<&str> = stored.project_name.as_deref();
        assert_eq!(
            plugin_arg,
            Some("resolved-proj"),
            "plugin spawn must receive the resolved project_name, not the request value",
        );
    }

    /// Companion test: when a non-project cwd is passed, project_name stays
    /// None and that's what the plugin layer sees.
    #[test]
    fn create_session_no_project_when_cwd_outside_project() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let state = mk_state();
        let cwd = Some(tmp.path().to_string_lossy().into_owned());
        let resp = create_session_impl(
            &state,
            &None,
            &None,
            &Some("sp".into()),
            &cwd,
            &None,
            0,
            &None,
            false,
            true,
            &None,
        );
        let id = match resp {
            crate::protocol::Response::SessionCreated { session_id } => session_id,
            other => panic!("expected SessionCreated, got {:?}", other),
        };
        let stored = {
            let st = lock_state(&state);
            st.db
                .get_session(&id)
                .expect("db.get_session")
                .expect("session row")
        };
        assert!(stored.project_name.is_none());
    }
}
