use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::notifications::{
    broadcast_to_subscribers, emit_phase, notify_parent_of_child_completion,
    notify_session_done_waiters, queue_and_maybe_resume, queue_info_to_session,
};
use super::registry::{maybe_respawn_for_queued, resolve_api_key};
use super::state::{SessionLocks, SharedState, lock_state, session_lock};
use super::tool_dispatch::handle_server_request;
use super::{SharedTestOverrides, ShutdownHandle};
use crate::compaction;
use crate::protocol::Response;
use crate::types::*;

/// Plugin-based tool executor for the agent loop.
pub(super) struct PluginExecutor {
    pub(super) plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    pub(super) state: SharedState,
    pub(super) session_locks: SessionLocks,
    /// Channel for spawning child Chat requests (session_id, text).
    /// Received by the server to spawn async agent turns.
    pub(super) chat_spawn_tx: smol::channel::Sender<(String, String)>,
    pub(super) shutdown: ShutdownHandle,
    pub(super) throttle: crate::throttle::ProviderThrottle,
    pub(super) session_id: String,
    pub(super) cwd: String,
    pub(super) project_name: Option<String>,
    pub(super) test_overrides: SharedTestOverrides,
}

#[async_trait::async_trait]
impl crate::worker::ToolExecutor for PluginExecutor {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        output_tx: &smol::channel::Sender<String>,
    ) -> crate::Result<ToolResultMessage> {
        // Take the plugin handle out of the manager (brief lock).
        // This lets us execute tool I/O without holding the PluginManager lock,
        // preventing deadlocks when tools make ServerRequest calls that need
        // to interact with other sessions (which also need plugin access).
        let taken = {
            let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
            pm.take_tool_plugin(&self.session_id, &tool_call.name)
        };
        let (mut handle, source) = match taken {
            Some(t) => t,
            None => {
                return Err(crate::Error::Io(format!(
                    "no plugin provides tool '{}'",
                    tool_call.name
                )));
            }
        };

        // Upgrade sync pipes to async for non-blocking I/O on the executor.
        if !handle.has_async_io()
            && let Err(e) = handle.upgrade_to_async()
        {
            // Return the (broken) handle before propagating error.
            let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
            pm.return_tool_plugin(source, handle);
            return Err(e);
        }

        // Send tool call to plugin.
        handle
            .send_async(&crate::plugin::PluginRequest::ToolCall {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                arguments: tool_call.arguments.clone(),
                cwd: Some(self.cwd.clone()),
                session_id: Some(self.session_id.clone()),
                project_name: self.project_name.clone(),
            })
            .await?;

        // Read messages from plugin until we get a ToolResult.
        let tool_call_for_hooks = tool_call.clone();
        let result = loop {
            let msg = handle.read_message_async().await?;
            match msg {
                crate::plugin::PluginMessage::OutputDelta { text, .. } => {
                    let _ = output_tx.send(text).await;
                }
                crate::plugin::PluginMessage::ToolResult(result) => {
                    break Ok(crate::types::ToolResultMessage {
                        tool_call_id: result.tool_call_id,
                        tool_name: tool_call.name.clone(),
                        content: result.content,
                        details: None,
                        is_error: result.is_error,
                        timestamp: crate::types::timestamp_ms(),
                        duration_ms: None,
                        summary: result.summary,
                    });
                }
                crate::plugin::PluginMessage::ServerRequest {
                    request_id,
                    request,
                } => {
                    let response = handle_server_request(
                        &self.state,
                        &self.session_locks,
                        &self.plugins,
                        &self.shutdown,
                        &self.throttle,
                        &self.chat_spawn_tx,
                        &self.test_overrides,
                        &request,
                        &self.session_id,
                    )
                    .await;
                    handle
                        .send_async(&crate::plugin::PluginRequest::ServerResponse {
                            request_id,
                            response,
                        })
                        .await?;
                }
                _ => {
                    // Ignore unexpected messages during tool execution
                }
            }
        };

        // Always return the plugin handle, even on error (brief lock).
        {
            let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
            pm.return_tool_plugin(source, handle);
        }

        // Run after_tool_hooks only on success.
        let mut result = result?;
        {
            let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
            pm.run_after_tool_hooks(&self.session_id, &tool_call_for_hooks, &mut result);
        }

        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_agent_turn<'a, W: futures::io::AsyncWrite + Unpin + Send + 'a>(
    state: &'a SharedState,
    plugins: &'a Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &'a ShutdownHandle,
    cancel_flag: Arc<AtomicBool>,
    model: &'a Model,
    context: &'a mut Context,
    cwd: &'a str,
    session_id: &'a str,
    writer: &'a mut W,
    throttle: &'a crate::throttle::ProviderThrottle,
    session_locks: &'a SessionLocks,
    test_overrides: &'a SharedTestOverrides,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::Result<crate::agent::AgentResult>> + Send + 'a>,
> {
    Box::pin(run_agent_turn_inner(
        state,
        plugins,
        shutdown,
        cancel_flag,
        model,
        context,
        cwd,
        session_id,
        writer,
        throttle,
        session_locks,
        test_overrides,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_turn_inner<W: futures::io::AsyncWrite + Unpin + Send>(
    state: &SharedState,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    cancel_flag: Arc<AtomicBool>,
    model: &Model,
    context: &mut Context,
    cwd: &str,
    session_id: &str,
    writer: &mut W,
    throttle: &crate::throttle::ProviderThrottle,
    session_locks: &SessionLocks,
    test_overrides: &SharedTestOverrides,
) -> crate::Result<crate::agent::AgentResult> {
    // Check provider throttle — sleep if rate limited
    if let Some(remaining) = throttle.check(&model.provider) {
        let human = crate::agent::format_duration_human(remaining.as_millis() as u64);
        eprintln!("provider '{}' throttled, waiting {}", model.provider, human);
        let msg = format!(
            "provider '{}' rate limited, retrying in {}...",
            model.provider, human
        );
        // Notify as a non-fatal status (not Error — Error would cause the TUI
        // to switch out of Streaming mode prematurely).
        let status_resp = Response::Stream {
            event: Box::new(StreamEvent::Status {
                message: msg.clone(),
            }),
        };
        send(writer, &status_resp).await.ok();
        broadcast_to_subscribers(state, session_id, &status_resp);
        // Emit rate-limited phase
        emit_phase(state, session_id, crate::types::AgentPhase::RateLimited);
        // Sleep with periodic cancellation checks
        let deadline = std::time::Instant::now() + remaining;
        while std::time::Instant::now() < deadline {
            if cancel_flag.load(Ordering::Relaxed) || shutdown.is_shutting_down() {
                return Err(crate::Error::Cancelled);
            }
            smol::Timer::after(std::time::Duration::from_secs(1)).await;
        }
    }

    let api_key = {
        let st = lock_state(state);
        resolve_api_key(&st.auth, &st.config, &model.provider)?
    };
    let api_key = match api_key {
        Some(key) => key,
        None => {
            return Err(crate::Error::NoApiKey(model.provider.clone()));
        }
    };

    let options = StreamOptions {
        api_key: Some(api_key),
        ..Default::default()
    };

    emit_phase(state, session_id, crate::types::AgentPhase::Connecting);

    let (event_tx, event_rx) = smol::channel::unbounded::<StreamEvent>();

    // Set up has_queued flag for this session
    let has_queued_flag = {
        let mut st = lock_state(state);
        st.has_queued
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };

    let shutdown_flag = shutdown.flag.clone();
    let cancel_flag_clone = cancel_flag.clone();
    let state_clone_persist = state.clone();
    let session_id_persist = session_id.to_string();
    let state_clone_drain = state.clone();
    let session_id_drain = session_id.to_string();
    let has_queued_clone = has_queued_flag.clone();
    let agent_config = crate::agent::AgentConfig {
        should_stop: Some(Box::new(move || {
            shutdown_flag.load(Ordering::Relaxed) || cancel_flag_clone.load(Ordering::Relaxed)
        })),
        drain_queued: Some(Box::new(move || {
            if has_queued_clone.swap(false, Ordering::Acquire) {
                let st = state_clone_drain.lock().expect("state mutex poisoned");
                st.db
                    .drain_queued_messages(&session_id_drain)
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        })),
        on_message: Some(std::sync::Mutex::new(Box::new(move |msg: &Message| {
            let st = state_clone_persist.lock().expect("state mutex poisoned");
            if let Err(e) = st.db.append_message(&session_id_persist, msg) {
                eprintln!("db error persisting agent message: {}", e);
            }
        }))),
        refresh_api_key: {
            let state_clone_refresh = state.clone();
            let provider_name = model.provider.clone();
            Some(Box::new(move || {
                let st = state_clone_refresh.lock().expect("state mutex poisoned");
                resolve_api_key(&st.auth, &st.config, &provider_name)
                    .ok()
                    .flatten()
            }))
        },
        idle_timeout_secs: std::env::var("TAU_STREAM_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::agent::AgentConfig::default().idle_timeout_secs),
        ..Default::default()
    };

    let registry_clone = {
        let st = lock_state(state);
        st.registry.clone()
    };
    let (child_budget, project_name) = {
        let st = lock_state(state);
        st.db
            .get_session(session_id)
            .ok()
            .flatten()
            .map(|s| (s.child_budget, s.project_name))
            .unwrap_or((0, None))
    };
    let plugin_tools = if !test_overrides.mock_tools.is_empty() {
        test_overrides.mock_tools.clone()
    } else {
        let pm = plugins.lock().expect("plugins mutex poisoned");
        pm.tool_schemas(session_id, child_budget)
    };

    let model_clone = model.clone();
    let options_clone = options;
    let cwd_clone = cwd.to_string();
    let mut context_clone = context.clone();

    let plugins_clone = plugins.clone();
    let state_clone_exec = state.clone();
    let session_locks_clone = session_locks.clone();
    let in_flight = shutdown.clone();
    let shutdown_clone = shutdown.clone();
    let throttle_clone = throttle.clone();
    let session_id_for_executor = session_id.to_string();
    let test_overrides_clone = test_overrides.clone();

    // Channel for child Chat requests spawned by orchestration tools.
    // The receiver task spawns async agent turns for each queued chat.
    let (chat_spawn_tx, chat_spawn_rx) = smol::channel::unbounded::<(String, String)>();

    // Spawn a task that processes queued child chats.
    let spawn_state = state.clone();
    let spawn_plugins = plugins.clone();
    let spawn_shutdown = shutdown.clone();
    let spawn_session_locks = session_locks.clone();
    let spawn_throttle = throttle.clone();
    let spawn_overrides = test_overrides.clone();
    smol::spawn(async move {
        while let Ok((child_session_id, text)) = chat_spawn_rx.recv().await {
            // Each child chat gets its own async task (fire-and-forget).
            let s = spawn_state.clone();
            let p = spawn_plugins.clone();
            let sh = spawn_shutdown.clone();
            let sl = spawn_session_locks.clone();
            let th = spawn_throttle.clone();
            let ov = spawn_overrides.clone();
            smol::spawn(async move {
                let sid = child_session_id;
                if let Err(e) = run_child_chat(s, p, sh, sl, th, sid.clone(), text, ov).await {
                    eprintln!("child chat {} error: {}", sid, e);
                }
            })
            .detach();
        }
    })
    .detach();

    let agent_handle = {
        async move {
            in_flight.enter();
            let mut executor: Box<dyn crate::worker::ToolExecutor> =
                if let Some(ref factory) = test_overrides_clone.tool_executor_factory {
                    factory()
                } else {
                    Box::new(PluginExecutor {
                        plugins: plugins_clone,
                        state: state_clone_exec,
                        session_locks: session_locks_clone,
                        chat_spawn_tx,
                        shutdown: shutdown_clone,
                        throttle: throttle_clone,
                        session_id: session_id_for_executor,
                        cwd: cwd_clone,
                        project_name,
                        test_overrides: test_overrides_clone.clone(),
                    })
                };
            let result = crate::agent::run(
                &registry_clone,
                &model_clone,
                &mut context_clone,
                &mut *executor,
                &options_clone,
                &agent_config,
                &plugin_tools,
                event_tx,
            )
            .await;
            in_flight.leave();
            result
        }
    };

    let state_clone = state.clone();
    let session_id_owned = session_id.to_string();
    let forward_handle = async {
        let mut writer_alive = true;
        while let Ok(event) = event_rx.recv().await {
            // Broadcast steering messages as UserMessage (persistence handled by on_message)
            if let StreamEvent::SteerMessage { ref message } = event {
                let text = message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let user_resp = Response::UserMessage { text };
                broadcast_to_subscribers(&state_clone, &session_id_owned, &user_resp);
                if writer_alive && send(writer, &user_resp).await.is_err() {
                    writer_alive = false;
                }
                continue;
            }
            // Update stored phase from implicit stream events.
            match &event {
                StreamEvent::ThinkingStart { .. } | StreamEvent::ThinkingDelta { .. } => {
                    let mut st = state_clone.lock().expect("state mutex poisoned");
                    st.phases
                        .insert(session_id_owned.clone(), crate::types::AgentPhase::Thinking);
                }
                StreamEvent::TextStart { .. }
                | StreamEvent::TextDelta { .. }
                | StreamEvent::ToolcallStart { .. } => {
                    let mut st = state_clone.lock().expect("state mutex poisoned");
                    st.phases.insert(
                        session_id_owned.clone(),
                        crate::types::AgentPhase::Responding,
                    );
                }
                StreamEvent::ToolcallEnd { .. } | StreamEvent::ToolResult { .. } => {
                    let mut st = state_clone.lock().expect("state mutex poisoned");
                    st.phases
                        .insert(session_id_owned.clone(), crate::types::AgentPhase::ToolExec);
                }
                _ => {}
            }
            let resp = Response::Stream {
                event: Box::new(event),
            };
            broadcast_to_subscribers(&state_clone, &session_id_owned, &resp);
            // Keep broadcasting even if the direct writer disconnected
            // (fire-and-forget clients close immediately).
            if writer_alive && send(writer, &resp).await.is_err() {
                writer_alive = false;
            }
        }
        Ok::<(), crate::Error>(())
    };

    let (agent_result, forward_result) = futures::future::join(agent_handle, forward_handle).await;
    if let Err(e) = forward_result {
        eprintln!("event forward error: {}", e);
    }

    let agent_result = agent_result?;

    Ok(agent_result)
}

/// Run an agent turn for a child session (spawned by orchestration tools).
/// This is a fire-and-forget async task -- output goes to subscribers only.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_child_chat(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    text: String,
    test_overrides: SharedTestOverrides,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        flag.clone()
    };

    let chat_result: Result<(bool, bool), crate::Error> = async {
        // Load session
        let (stored, mut messages, cwd) = {
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
        }?;
        let model = stored.model.clone();

        // Ensure session plugins
        {
            let mut pm = plugins.lock().expect("plugins mutex poisoned");
            match pm.ensure_session_plugins(&session_id, &cwd, stored.project_name.as_deref()) {
                Ok(failures) => {
                    for msg in &failures {
                        queue_info_to_session(&state, &session_id, msg);
                    }
                }
                Err(e) => eprintln!("child session {} plugin spawn error: {}", session_id, e),
            }
            pm.notify_session_start_once(&cwd, &session_id, stored.project_name.as_deref());
        }

        // Build system prompt if not set
        let system_prompt = stored.system_prompt.clone().or_else(|| {
            let pm = plugins.lock().expect("plugins mutex poisoned");
            let tool_prompts = pm.tool_prompts(&session_id, stored.child_budget);
            Some(crate::system_prompt::build(
                &crate::system_prompt::PromptOptions {
                    cwd: Some(cwd.clone()),
                    tools: tool_prompts,
                    ..Default::default()
                },
            ))
        });

        // Append user message
        let user_msg = Message::User(UserMessage::text(&text));
        {
            let st = lock_state(&state);
            st.db.append_message(&session_id, &user_msg)?;
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

        // Use a sink writer that discards output (no direct client connection).
        let mut sink = futures::io::sink();
        let result = run_agent_turn(
            &state,
            &plugins,
            &shutdown,
            cancel_flag.clone(),
            &model,
            &mut context,
            &cwd,
            &session_id,
            &mut sink,
            &throttle,
            &session_locks,
            &test_overrides,
        )
        .await;

        let max_turns_reached = match result {
            Ok(ref agent_result) => agent_result.max_turns_reached,
            Err(crate::Error::Cancelled) => {
                cancel_flag.store(true, Ordering::Relaxed);
                false
            }
            Err(e) => return Err(e),
        };

        Ok((cancel_flag.load(Ordering::Relaxed), max_turns_reached))
    }
    .await;

    // Broadcast terminal response and notify parent.
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers(&state, &session_id, &Response::Cancelled);
            // Notify parent about cancellation.
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                "cancelled",
                None,
                &test_overrides,
            );
        }
        Ok((false, max_turns_reached)) => {
            if max_turns_reached {
                // Notify the parent session that this child hit its step limit.
                let parent_id = {
                    let st = lock_state(&state);
                    st.db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.parent_id)
                };
                if let Some(pid) = parent_id {
                    let notice = format!(
                        "Child session {} reached its tool use limit. \
                         Use session_read to check progress and send a follow-up message to continue, \
                         or session_cancel to stop it.",
                        session_id
                    );
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &pid,
                        &notice,
                        &format!("child:{}", session_id),
                        &test_overrides,
                    );
                }
            } else {
                // Normal completion -- notify parent.
                notify_parent_of_child_completion(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    "completed",
                    None,
                    &test_overrides,
                );
            }
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
        Err(ref e) => {
            let err_msg = format!("child agent error: {}", e);
            broadcast_to_subscribers(
                &state,
                &session_id,
                &Response::Error {
                    message: err_msg.clone(),
                },
            );
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
            // Notify parent about error.
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                &format!("error: {}", e),
                None,
                &test_overrides,
            );
        }
    }

    emit_phase(&state, &session_id, crate::types::AgentPhase::Idle);
    notify_session_done_waiters(&state);

    // Before the session lock drops, check whether new messages arrived while
    // we were in post-turn cleanup (broadcast / notify / emit_phase).  If so,
    // spawn a resume task — it will acquire the lock as soon as we drop ours.
    maybe_respawn_for_queued(
        &state,
        &session_locks,
        &plugins,
        &shutdown,
        &throttle,
        &session_id,
        &test_overrides,
    );

    Ok(())
}

/// Resume an interrupted child session. Unlike `run_child_chat`, this does
/// not append a new user message — it just runs the agent on the existing
/// message history. Used for auto-resume on server restart.
pub(super) async fn resume_child_session(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    test_overrides: SharedTestOverrides,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        flag.clone()
    };

    let chat_result: Result<(bool, bool), crate::Error> = async {
        // Load session
        let (stored, mut messages, cwd) = {
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
        }?;
        let model = stored.model.clone();

        // Ensure session plugins
        {
            let mut pm = plugins.lock().expect("plugins mutex poisoned");
            match pm.ensure_session_plugins(&session_id, &cwd, stored.project_name.as_deref()) {
                Ok(failures) => {
                    for msg in &failures {
                        queue_info_to_session(&state, &session_id, msg);
                    }
                }
                Err(e) => eprintln!("resume session {} plugin spawn error: {}", session_id, e),
            }
            pm.notify_session_start_once(&cwd, &session_id, stored.project_name.as_deref());
        }

        // Repair any corrupted message history
        let repair_stubs = crate::agent::repair_messages(&messages);
        if !repair_stubs.is_empty() {
            eprintln!(
                "session {}: repaired {} missing tool_result message(s)",
                session_id,
                repair_stubs.len()
            );
            let st = lock_state(&state);
            for stub in &repair_stubs {
                if let Err(e) = st.db.append_message(&session_id, stub) {
                    eprintln!("db error persisting repair stub: {}", e);
                }
            }
            messages.extend(repair_stubs);
        }

        // Build system prompt if not set
        let system_prompt = stored.system_prompt.clone().or_else(|| {
            let pm = plugins.lock().expect("plugins mutex poisoned");
            let tool_prompts = pm.tool_prompts(&session_id, stored.child_budget);
            Some(crate::system_prompt::build(
                &crate::system_prompt::PromptOptions {
                    cwd: Some(cwd.clone()),
                    tools: tool_prompts,
                    ..Default::default()
                },
            ))
        });

        // No user message appended — resume on existing messages.
        let mut context = Context {
            system_prompt,
            messages,
            tools: Vec::new(),
        };

        // Use a sink writer (no direct client connection).
        let mut sink = futures::io::sink();
        let result = run_agent_turn(
            &state,
            &plugins,
            &shutdown,
            cancel_flag.clone(),
            &model,
            &mut context,
            &cwd,
            &session_id,
            &mut sink,
            &throttle,
            &session_locks,
            &test_overrides,
        )
        .await;

        let max_turns_reached = match result {
            Ok(ref agent_result) => agent_result.max_turns_reached,
            Err(crate::Error::Cancelled) => {
                cancel_flag.store(true, Ordering::Relaxed);
                false
            }
            Err(e) => return Err(e),
        };

        Ok((cancel_flag.load(Ordering::Relaxed), max_turns_reached))
    }
    .await;

    // Broadcast terminal response and notify parent (same as run_child_chat).
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers(&state, &session_id, &Response::Cancelled);
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                "cancelled",
                None,
                &test_overrides,
            );
        }
        Ok((false, max_turns_reached)) => {
            if max_turns_reached {
                let parent_id = {
                    let st = lock_state(&state);
                    st.db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.parent_id)
                };
                if let Some(pid) = parent_id {
                    let notice = format!(
                        "Child session {} reached its tool use limit. \
                         Use session_read to check progress and send a follow-up message to continue, \
                         or session_cancel to stop it.",
                        session_id
                    );
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &pid,
                        &notice,
                        &format!("child:{}", session_id),
                        &test_overrides,
                    );
                }
            } else {
                notify_parent_of_child_completion(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    "completed",
                    None,
                    &test_overrides,
                );
            }
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
        Err(ref e) => {
            let err_msg = format!("child agent error: {}", e);
            broadcast_to_subscribers(
                &state,
                &session_id,
                &Response::Error {
                    message: err_msg.clone(),
                },
            );
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                &format!("error: {}", e),
                None,
                &test_overrides,
            );
        }
    }

    emit_phase(&state, &session_id, crate::types::AgentPhase::Idle);
    notify_session_done_waiters(&state);

    // Before the session lock drops, check whether new messages arrived while
    // we were in post-turn cleanup.  See `maybe_respawn_for_queued` doc.
    maybe_respawn_for_queued(
        &state,
        &session_locks,
        &plugins,
        &shutdown,
        &throttle,
        &session_id,
        &test_overrides,
    );

    Ok(())
}

pub(super) async fn run_compaction<W: futures::io::AsyncWrite + Unpin>(
    state: &SharedState,
    session_id: &str,
    model: &Model,
    writer: &mut W,
) -> crate::Result<()> {
    emit_phase(state, session_id, crate::types::AgentPhase::Compacting);

    let settings = compaction::CompactionSettings::default();

    // Load messages and find cut point
    let (messages, cut_idx) = {
        let st = lock_state(state);
        let messages = st.db.get_messages(session_id)?;
        let cut = compaction::find_cut_point(&messages, settings.keep_recent_tokens);
        (messages, cut)
    };

    if cut_idx == 0 {
        return Ok(()); // Nothing to compact
    }

    let messages_to_summarize = &messages[..cut_idx];
    let ctx_before = compaction::estimate_context_tokens(&messages);

    // Notify client
    send(
        writer,
        &Response::Error {
            message: format!(
                "compacting session ({} messages → summary)...",
                messages_to_summarize.len()
            ),
        },
    )
    .await?;

    // Build summarization context and call LLM
    let summary_ctx = compaction::build_summarization_context(messages_to_summarize);

    let api_key = {
        let st = lock_state(state);
        resolve_api_key(&st.auth, &st.config, &model.provider)?
    };

    let options = StreamOptions {
        api_key,
        max_tokens: Some(settings.reserve_tokens),
        ..Default::default()
    };

    let rx = {
        let st = lock_state(state);
        st.registry.stream(model, &summary_ctx, &options)?
    };

    // Wait for summary (blocking on the channel)
    let summary = smol::unblock({
        let rx = rx.clone();
        move || compaction::extract_summary(&rx)
    })
    .await?;

    // Get the DB row ID of the first kept message
    let keep_from_id = {
        let st = lock_state(state);
        st.db
            .get_message_row_id(session_id, cut_idx)?
            .ok_or_else(|| crate::Error::Io("cut point message not found".into()))?
    };

    // Perform compaction in DB
    {
        let st = lock_state(state);
        st.db
            .compact_session(session_id, &summary, keep_from_id, ctx_before)?;
    }

    let after_tokens = {
        let st = lock_state(state);
        let messages = st.db.get_messages(session_id)?;
        compaction::estimate_context_tokens(&messages)
    };

    send(
        writer,
        &Response::Error {
            message: format!("compaction done: {} → {} tokens", ctx_before, after_tokens),
        },
    )
    .await?;

    Ok(())
}

pub(super) async fn send<W: futures::io::AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> crate::Result<()> {
    crate::write_json_line_async(writer, resp).await
}
