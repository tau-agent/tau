use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::notifications::{
    broadcast_to_subscribers, broadcast_to_subscribers_and_wait, emit_phase, emit_phase_and_wait,
    notify_parent_of_child_completion, notify_session_done_waiters, queue_and_maybe_resume,
    queue_info_to_session,
};
use super::registry::{maybe_respawn_for_queued, resolve_api_key, resolve_api_key_excluding};
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
    /// Channel for spawning child Chat requests. Carries the full chat
    /// payload (text + attachments) so children inherit images attached
    /// by the parent's orchestration tool.
    /// Received by the server to spawn async agent turns.
    pub(super) chat_spawn_tx: smol::channel::Sender<super::state::ChatSpawn>,
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
        cancel: &tau_agent_base::types::CancelToken,
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

        // Set up a per-tool-call writer task so multiple concurrent senders
        // (main loop, ServerResponse path, cancel watcher) can write to the
        // plugin's stdin without contending on a shared `&mut AsyncWrite`.
        //
        // For session plugins (which includes the worker plugin running
        // bash) there is no long-lived background writer — `set_background_channels`
        // is only ever called for global plugins. Installing an ephemeral
        // writer task here is what lets the cancel RPC reach the plugin
        // while the main loop is blocked inside `read_message_async`.
        //
        // If the handle already has background channels installed (global
        // plugin path), reuse its existing write channel and skip spawning
        // a local writer task.
        let (write_tx, writer_task): (
            smol::channel::Sender<crate::plugin::PluginRequest>,
            Option<smol::Task<crate::Result<crate::plugin::AsyncPluginWriter>>>,
        ) = if let Some(tx) = handle.background_write_tx() {
            (tx, None)
        } else {
            let (tx, rx) = smol::channel::unbounded::<crate::plugin::PluginRequest>();
            let mut writer = match handle.take_async_writer() {
                Ok(w) => w,
                Err(e) => {
                    let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
                    pm.return_tool_plugin(source, handle);
                    return Err(e);
                }
            };
            let plugin_name = handle.name.clone();
            let task = smol::spawn(async move {
                while let Ok(req) = rx.recv().await {
                    if let Err(e) = crate::write_json_line_async(&mut writer, &req).await {
                        tracing::warn!(
                            plugin = %plugin_name,
                            %e,
                            "tool-call writer task error"
                        );
                        break;
                    }
                }
                // Return the writer so the caller can put it back on the handle.
                Ok(writer)
            });
            (tx, Some(task))
        };

        // Send tool call to plugin via the shared write channel.
        if let Err(e) = write_tx
            .send(crate::plugin::PluginRequest::ToolCall {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                arguments: tool_call.arguments.clone(),
                cwd: Some(self.cwd.clone()),
                session_id: Some(self.session_id.clone()),
                project_name: self.project_name.clone(),
            })
            .await
        {
            drop(write_tx);
            if let Some(task) = writer_task
                && let Ok(w) = task.await
            {
                handle.restore_async_writer(w);
            }
            let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
            pm.return_tool_plugin(source, handle);
            return Err(crate::Error::Io(format!("write channel closed: {}", e)));
        }

        // Spawn a cancel-watcher task: when the cancel token is set, send a
        // CancelToolCall to the plugin so it can abort the in-flight tool
        // (e.g. SIGKILL the bash process group). Without this hop the plugin
        // would keep running the tool until the watchdog or natural exit.
        let cancel_clone = cancel.clone();
        let cancel_tool_call_id = tool_call.id.clone();
        let cancel_write_tx = write_tx.clone();
        let cancel_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_done_for_task = cancel_done.clone();
        let cancel_watcher = smol::spawn(async move {
            loop {
                if cancel_done_for_task.load(Ordering::Relaxed) {
                    return;
                }
                if cancel_clone.is_cancelled() {
                    let _ = cancel_write_tx
                        .send(crate::plugin::PluginRequest::CancelToolCall {
                            tool_call_id: cancel_tool_call_id.clone(),
                        })
                        .await;
                    return;
                }
                smol::Timer::after(std::time::Duration::from_millis(100)).await;
            }
        });

        // Read messages from plugin until we get a ToolResult.
        let tool_call_for_hooks = tool_call.clone();
        let result: crate::Result<crate::types::ToolResultMessage> = loop {
            let msg = match handle.read_message_async().await {
                Ok(m) => m,
                Err(e) => break Err(e),
            };
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
                        post_persist_actions: result.post_persist_actions,
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
                    let _ = write_tx
                        .send(crate::plugin::PluginRequest::ServerResponse {
                            request_id,
                            response,
                        })
                        .await;
                }
                _ => {
                    // Ignore unexpected messages during tool execution
                }
            }
        };

        // Tool call is done — tell the cancel watcher to exit.
        cancel_done.store(true, Ordering::Relaxed);
        cancel_watcher.cancel().await;

        // Close the per-call write channel so the writer task drains and
        // exits, then restore the async writer to the handle so future
        // PluginExecutor::execute calls against this same handle can reuse
        // it. For the bg-channel path (global plugins) there's no writer
        // task and no writer to restore.
        drop(write_tx);
        if let Some(task) = writer_task {
            match task.await {
                Ok(writer) => handle.restore_async_writer(writer),
                Err(e) => tracing::warn!(%e, "tool-call writer task error"),
            }
        }

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
        tracing::info!(provider = %model.provider, wait = %human, "provider throttled");
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

    // Preflight: resolve API key unless the provider is a no-key provider
    // (e.g. the `log` provider). This is the P1 safety net from task 582 —
    // even if an agent loop somehow kicks off on a log-provider session we
    // do NOT want to emit "no API key for provider: log".
    let needs_key = {
        let st = lock_state(state);
        st.registry.needs_api_key(&model.api)
    };
    let api_key = if needs_key {
        let api_key = {
            let st = lock_state(state);
            resolve_api_key(&st.auth, &st.config, &model.provider)?
        };
        match api_key {
            Some(key) => Some(key),
            None => {
                tracing::error!(
                    session_id = %session_id,
                    model = %model.id,
                    provider = %model.provider,
                    ts_ms = crate::types::timestamp_ms(),
                    "agent_runner: NoApiKey early-return — see resolve_api_key warning above"
                );
                return Err(crate::Error::NoApiKey(model.provider.clone()));
            }
        }
    } else {
        None
    };

    let options = StreamOptions {
        api_key,
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
        cancel_token: Some(tau_agent_base::types::CancelToken::from_flag(
            cancel_flag.clone(),
        )),
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
                tracing::warn!(%e, "db error persisting agent message");
            }
        }))),
        post_persist_callback: {
            let state_clone_pp = state.clone();
            Some(Box::new(
                move |actions: &[tau_agent_base::types::PostPersistAction]| {
                    for action in actions {
                        match action {
                            tau_agent_base::types::PostPersistAction::EmitInfoMessage {
                                target_session_id,
                                text,
                            } => {
                                super::notifications::queue_info_to_session(
                                    &state_clone_pp,
                                    target_session_id,
                                    text,
                                );
                            }
                        }
                    }
                },
            ))
        },
        refresh_api_key: {
            let state_clone_refresh = state.clone();
            let provider_name = model.provider.clone();
            Some(Box::new(move |stale: Option<&str>| {
                let st = state_clone_refresh.lock().expect("state mutex poisoned");
                resolve_api_key_excluding(&st.auth, &st.config, &provider_name, stale)
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
    let (chat_spawn_tx, chat_spawn_rx) = smol::channel::unbounded::<super::state::ChatSpawn>();

    // Spawn a task that processes queued child chats.
    let spawn_state = state.clone();
    let spawn_plugins = plugins.clone();
    let spawn_shutdown = shutdown.clone();
    let spawn_session_locks = session_locks.clone();
    let spawn_throttle = throttle.clone();
    let spawn_overrides = test_overrides.clone();
    smol::spawn(async move {
        while let Ok(spawn) = chat_spawn_rx.recv().await {
            // Each child chat gets its own async task (fire-and-forget).
            let s = spawn_state.clone();
            let p = spawn_plugins.clone();
            let sh = spawn_shutdown.clone();
            let sl = spawn_session_locks.clone();
            let th = spawn_throttle.clone();
            let ov = spawn_overrides.clone();
            smol::spawn(async move {
                let super::state::ChatSpawn {
                    session_id,
                    text,
                    attachments,
                } = spawn;
                let sid = session_id;
                if let Err(e) =
                    run_child_chat(s, p, sh, sl, th, sid.clone(), text, attachments, ov).await
                {
                    tracing::warn!(session_id = %sid, %e, "child chat error");
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
            // Update stored phase from implicit stream events and, for
            // explicit `Phase` events emitted by the engine, rebuild the
            // event with a server-stamped `turn_started_at_ms` anchor.
            let event = match event {
                StreamEvent::ThinkingStart { .. } | StreamEvent::ThinkingDelta { .. } => {
                    super::notifications::set_phase_and_stamp(
                        &state_clone,
                        &session_id_owned,
                        crate::types::AgentPhase::Thinking,
                    );
                    event
                }
                StreamEvent::TextStart { .. }
                | StreamEvent::TextDelta { .. }
                | StreamEvent::ToolcallStart { .. } => {
                    super::notifications::set_phase_and_stamp(
                        &state_clone,
                        &session_id_owned,
                        crate::types::AgentPhase::Responding,
                    );
                    event
                }
                StreamEvent::ToolcallEnd { .. } | StreamEvent::ToolResult { .. } => {
                    super::notifications::set_phase_and_stamp(
                        &state_clone,
                        &session_id_owned,
                        crate::types::AgentPhase::ToolExec,
                    );
                    event
                }
                StreamEvent::Phase { phase, .. } => {
                    // Engine-emitted phase events don't carry a timestamp
                    // (engine is wire-agnostic). Stamp on forward.
                    let (turn_ts, phase_ts) = super::notifications::set_phase_and_stamp(
                        &state_clone,
                        &session_id_owned,
                        phase,
                    );
                    StreamEvent::Phase {
                        phase,
                        turn_started_at_ms: turn_ts,
                        phase_started_at_ms: phase_ts,
                    }
                }
                other => other,
            };
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
        tracing::warn!(%e, "event forward error");
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
    attachments: Vec<crate::protocol::ChatAttachment>,
    test_overrides: SharedTestOverrides,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag and mark session as live.
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        let flag = flag.clone();
        st.live_sessions.insert(session_id.clone());
        flag
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
            match pm.ensure_session_plugins(&session_id, &cwd, stored.project_name.as_deref(), None)
            {
                Ok(failures) => {
                    for msg in &failures {
                        queue_info_to_session(&state, &session_id, msg);
                    }
                }
                Err(e) => tracing::warn!(
                    session_id = %session_id,
                    %e,
                    "child session plugin spawn error"
                ),
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

        // Append user message — use the shared chat-attachment builder so
        // images attached upstream survive the trip into engine context.
        let user_msg =
            match super::chat_attachments::build_user_message_for_request(&text, &attachments) {
                Ok(m) => m,
                Err(e) => {
                    return Err(crate::Error::Io(format!("invalid chat attachments: {}", e)));
                }
            };
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

    // Broadcast terminal response and notify parent. Terminal broadcasts use
    // the awaiting variant so subscribers observe them before the session
    // transitions to idle via another code path.
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::Cancelled).await;
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
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::AgentDone).await;
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
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::AgentDone).await;
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

    // Terminal Idle — await so the TUI sees it settle before any other
    // idle-indicator path (session lock released, live_sessions cleared).
    emit_phase_and_wait(&state, &session_id, crate::types::AgentPhase::Idle).await;
    // Mark session as no longer live.
    {
        let mut st = lock_state(&state);
        st.live_sessions.remove(&session_id);
    }
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

    // Tier-3 post-idle drain: release the session lock, then run queued
    // post-idle actions (archive caller's subtree, etc.).
    drop(_session_guard);
    super::post_idle::drain(&state, &session_id).await;

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

    // Set up cancel flag and mark session as live.
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        let flag = flag.clone();
        st.live_sessions.insert(session_id.clone());
        flag
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
            match pm.ensure_session_plugins(&session_id, &cwd, stored.project_name.as_deref(), None)
            {
                Ok(failures) => {
                    for msg in &failures {
                        queue_info_to_session(&state, &session_id, msg);
                    }
                }
                Err(e) => tracing::warn!(
                    session_id = %session_id,
                    %e,
                    "resume session plugin spawn error"
                ),
            }
            pm.notify_session_start_once(&cwd, &session_id, stored.project_name.as_deref());
        }

        // Repair any corrupted message history
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
    // Terminal broadcasts are awaited; see module comment in notifications.rs.
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::Cancelled).await;
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
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::AgentDone).await;
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
            broadcast_to_subscribers_and_wait(&state, &session_id, &Response::AgentDone).await;
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

    emit_phase_and_wait(&state, &session_id, crate::types::AgentPhase::Idle).await;
    // Mark session as no longer live.
    {
        let mut st = lock_state(&state);
        st.live_sessions.remove(&session_id);
    }
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

    // Tier-3 post-idle drain: release the session lock, then run queued
    // post-idle actions (archive caller's subtree, etc.).
    drop(_session_guard);
    super::post_idle::drain(&state, &session_id).await;

    Ok(())
}

pub(super) async fn run_compaction(
    state: &SharedState,
    session_id: &str,
    model: &Model,
    keep_hint: Option<&str>,
    manual: bool,
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
        // Nothing meaningful to compact. Auto-compaction silently no-ops; for
        // a manual `/compact` request, broadcast a Status so any subscriber
        // (the requesting TUI plus any others watching) sees that the
        // command was received and why nothing happened, and persist a
        // matching Info message so there's a durable record in the
        // transcript. Auto-compaction (manual=false) stays silent: it
        // shouldn't spam the transcript with no-op notes.
        if manual {
            let text = "manual compaction: nothing to compact yet \
                        (history fits within keep-recent window)";
            let info = Response::Stream {
                event: Box::new(crate::types::StreamEvent::Status {
                    message: text.to_string(),
                }),
            };
            broadcast_to_subscribers(state, session_id, &info);
            queue_info_to_session(state, session_id, text);
        }
        return Ok(());
    }

    let messages_to_summarize = &messages[..cut_idx];
    let ctx_before = compaction::estimate_context_tokens(&messages);

    // Notify subscribers that compaction is starting. The requesting
    // client (TUI) is also a subscriber on its Subscribe connection, so
    // it sees this Status alongside any other attached subscribers.
    {
        let progress = Response::Stream {
            event: Box::new(crate::types::StreamEvent::Status {
                message: format!(
                    "compacting session ({} messages \u{2192} summary)...",
                    messages_to_summarize.len()
                ),
            }),
        };
        broadcast_to_subscribers(state, session_id, &progress);
    }

    // Build summarization context and call LLM
    let summary_ctx = compaction::build_summarization_context(messages_to_summarize, keep_hint);

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

    let done_text = format!(
        "compaction done: {} \u{2192} {} tokens",
        ctx_before, after_tokens
    );
    let done = Response::Stream {
        event: Box::new(crate::types::StreamEvent::Status {
            message: done_text.clone(),
        }),
    };
    broadcast_to_subscribers(state, session_id, &done);
    if manual {
        // Persist a durable record of the outcome to the transcript so a
        // user asking "did /compact work?" days later can find evidence
        // even if they missed the live Status.
        queue_info_to_session(state, session_id, &done_text);
    }

    Ok(())
}

pub(super) async fn send<W: futures::io::AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> crate::Result<()> {
    crate::write_json_line_async(writer, resp).await
}

#[cfg(test)]
mod compaction_tests {
    //! Regression tests for task #875: compaction must broadcast progress /
    //! outcome to subscribers and persist a durable Info message to the
    //! transcript so a user can answer "did /compact actually work?" days
    //! later.

    use super::*;
    use crate::db::{Db, StoredSession};
    use crate::provider::ProviderRegistry;
    use crate::server::state::State;
    use std::collections::{HashMap, HashSet};
    use tau_agent_engine::providers::mock::{MockProvider, MockResponse, mock_model};

    fn mk_state_with_mock() -> SharedState {
        let db = Db::open_memory().expect("open in-memory db");
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::new(vec![MockResponse::Text(
            "## Summary\n- did stuff\n".to_string(),
        )]));
        let model = mock_model();
        Arc::new(Mutex::new(State {
            db,
            registry,
            auth: crate::auth::AuthStorage::open_default(),
            config: crate::config::Config::default(),
            global_aliases: HashMap::new(),
            default_model: model.clone(),
            all_models: vec![model],
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

    fn seed_compactable_session(state: &SharedState, session_id: &str, model: &Model) {
        let st = lock_state(state);
        st.db
            .create_session(&StoredSession {
                id: session_id.to_string(),
                model: model.clone(),
                system_prompt: None,
                cwd: None,
                is_subscription: false,
                created_at: 0,
                parent_id: None,
                child_budget: 0,
                tagline: None,
                archived: false,
                last_exit_status: None,
                last_phase: None,
                auto_archive: false,
                notify_parent: true,
                project_name: None,
            })
            .expect("create session");

        // Build a transcript whose backwards walk forces find_cut_point to
        // land at message index 2 (the second User). The big assistant blob
        // ensures `keep_recent_tokens=20_000` is exceeded before we walk
        // past it.
        let big_text: String = "x".repeat(120_000); // ~30k token estimate
        let messages = vec![
            Message::User(UserMessage::text("hello")),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: big_text,
                    text_signature: None,
                })],
                api: "mock".into(),
                provider: "mock".into(),
                model: model.id.clone(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            }),
            Message::User(UserMessage::text("follow up")),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                api: "mock".into(),
                provider: "mock".into(),
                model: model.id.clone(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            }),
        ];
        for msg in &messages {
            st.db
                .append_message(session_id, msg)
                .expect("append message");
        }
    }

    /// Acceptance criterion #6: a successful manual compaction must (a)
    /// persist a `compaction_summary` row in the DB, (b) persist an Info
    /// message with "compaction done" text to the transcript, and (c)
    /// broadcast a Status event with that text to attached subscribers.
    #[test]
    fn run_compaction_persists_info_and_broadcasts_status() {
        smol::block_on(async {
            let state = mk_state_with_mock();
            let model = mock_model();
            let session_id = "s-compact-test";
            seed_compactable_session(&state, session_id, &model);

            // Register a subscriber on the session so we can capture the
            // Status broadcasts emitted by run_compaction.
            let (tx, rx) = smol::channel::unbounded::<Response>();
            {
                let mut st = lock_state(&state);
                st.subscribers.insert(session_id.into(), vec![tx]);
            }

            run_compaction(&state, session_id, &model, None, /* manual */ true)
                .await
                .expect("run_compaction");

            // -- (a) compaction_summary row present.
            let messages_after = {
                let st = lock_state(&state);
                st.db.get_messages(session_id).expect("get messages")
            };
            assert!(
                messages_after
                    .iter()
                    .any(|m| matches!(m, Message::CompactionSummary(_))),
                "expected a CompactionSummary in the transcript, got: {:?}",
                messages_after
            );

            // -- (b) Info message recording the outcome is present.
            let saw_info_done = messages_after.iter().any(|m| {
                matches!(
                    m,
                    Message::Info(info) if info.text.contains("compaction done")
                )
            });
            assert!(
                saw_info_done,
                "expected an Info(\"compaction done...\") message after manual compaction, \
                 got: {:?}",
                messages_after
            );

            // -- (c) Subscriber received progress + completion Status
            //        broadcasts.
            let mut collected = Vec::new();
            while let Ok(resp) = rx.try_recv() {
                collected.push(resp);
            }
            let saw_progress = collected.iter().any(|r| {
                matches!(
                    r,
                    Response::Stream { event }
                        if matches!(
                            event.as_ref(),
                            StreamEvent::Status { message } if message.contains("compacting session")
                        )
                )
            });
            let saw_done = collected.iter().any(|r| {
                matches!(
                    r,
                    Response::Stream { event }
                        if matches!(
                            event.as_ref(),
                            StreamEvent::Status { message } if message.contains("compaction done")
                        )
                )
            });
            assert!(
                saw_progress,
                "subscriber should have observed a 'compacting session' Status, got: {:?}",
                collected
            );
            assert!(
                saw_done,
                "subscriber should have observed a 'compaction done' Status, got: {:?}",
                collected
            );
        });
    }

    /// Auto-compaction (manual=false) must NOT persist an Info message on
    /// the no-op path — that would spam the transcript every turn.
    #[test]
    fn auto_compaction_no_op_does_not_persist_info() {
        smol::block_on(async {
            let state = mk_state_with_mock();
            let model = mock_model();
            let session_id = "s-auto-noop";
            // Seed a session with no compactable prefix — just one turn.
            {
                let st = lock_state(&state);
                st.db
                    .create_session(&StoredSession {
                        id: session_id.to_string(),
                        model: model.clone(),
                        system_prompt: None,
                        cwd: None,
                        is_subscription: false,
                        created_at: 0,
                        parent_id: None,
                        child_budget: 0,
                        tagline: None,
                        archived: false,
                        last_exit_status: None,
                        last_phase: None,
                        auto_archive: false,
                        notify_parent: true,
                        project_name: None,
                    })
                    .expect("create session");
                st.db
                    .append_message(session_id, &Message::User(UserMessage::text("hi")))
                    .expect("append user");
            }

            run_compaction(&state, session_id, &model, None, /* manual */ false)
                .await
                .expect("run_compaction");

            let messages_after = {
                let st = lock_state(&state);
                st.db.get_messages(session_id).expect("get messages")
            };
            assert!(
                !messages_after.iter().any(|m| matches!(m, Message::Info(_))),
                "auto-compaction must not persist Info messages on the no-op path, got: {:?}",
                messages_after
            );
        });
    }
}
