use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::agent_runner::{resume_child_session, run_child_chat};
use super::state::SessionLocks;
use super::state::{SharedState, lock_state};
use super::tool_dispatch::handle_server_request;
use super::{SharedTestOverrides, ShutdownHandle, TestOverrides};
use crate::protocol::{ModelInfo, SessionInfo, SessionStats, TokenStats};
use crate::provider::ProviderRegistry;
use crate::types::*;

// ---------------------------------------------------------------------------
// Compute stats from a message list
// ---------------------------------------------------------------------------

pub(super) fn compute_stats(
    messages: &[Message],
    model: &Model,
    is_subscription: bool,
) -> SessionStats {
    let mut user_messages = 0usize;
    let mut assistant_messages = 0usize;
    let mut tool_calls = 0usize;
    let mut tool_results = 0usize;
    let mut tokens = TokenStats::default();
    let mut cost = 0.0f64;
    let mut last_input_tokens: Option<u64> = None;

    for msg in messages {
        match msg {
            Message::User(_) => user_messages += 1,
            Message::Assistant(a) => {
                assistant_messages += 1;
                for c in &a.content {
                    if matches!(c, AssistantContent::ToolCall(_)) {
                        tool_calls += 1;
                    }
                }
                tokens.input += a.usage.input;
                tokens.output += a.usage.output;
                tokens.cache_read += a.usage.cache_read;
                tokens.cache_write += a.usage.cache_write;
                cost += a.usage.cost.total;

                if a.stop_reason != StopReason::Error && a.stop_reason != StopReason::Aborted {
                    last_input_tokens =
                        Some(a.usage.input + a.usage.cache_read + a.usage.cache_write);
                }
            }
            Message::ToolResult(_) => tool_results += 1,
            Message::CompactionSummary(_) => {}
            Message::Info(_) => {}
        }
    }

    SessionStats {
        user_messages,
        assistant_messages,
        tool_calls,
        tool_results,
        tokens,
        cost,
        is_subscription,
        context_window: model.context_window,
        context_tokens: last_input_tokens,
    }
}

pub(super) fn session_info(
    stored: &crate::db::StoredSession,
    messages: &[Message],
    last_message_time: Option<i64>,
    child_count: usize,
    phase: Option<&(crate::types::AgentPhase, Option<u64>, Option<u64>)>,
    is_live: bool,
) -> SessionInfo {
    let stats = compute_stats(messages, &stored.model, stored.is_subscription);
    let context_pct = if stats.context_window > 0 {
        stats
            .context_tokens
            .map(|t| (t as f64 / stats.context_window as f64) * 100.0)
    } else {
        None
    };
    let phase_value = phase.map(|p| p.0);
    let turn_started_at_ms = phase.and_then(|p| p.1);
    let phase_started_at_ms = phase.and_then(|p| p.2);
    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: messages.len(),
        stats,
        // Timestamps in DB are milliseconds; convert to seconds for display
        last_activity: last_message_time.unwrap_or(stored.created_at) / 1000,
        parent_id: stored.parent_id.clone(),
        child_count,
        child_budget: stored.child_budget,
        tagline: stored.tagline.clone(),
        state: phase_value
            .unwrap_or_default()
            .label()
            .trim_end_matches("...")
            .to_string(),
        context_pct,
        archived: stored.archived,
        last_exit_status: stored.last_exit_status.clone(),
        is_live,
        project_name: stored.project_name.clone(),
        successor_id: stored.successor_id.clone(),
        turn_started_at_ms,
        phase_started_at_ms,
    }
}

/// Build a `SessionInfo` from pre-computed DB-level stats (no message
/// deserialisation).  Used by `list_sessions_impl` for O(1)-per-session cost.
pub(super) fn session_info_from_db_stats(
    stored: &crate::db::StoredSession,
    db_stats: Option<&crate::db::DbSessionStats>,
    child_count: usize,
    phase: Option<&(crate::types::AgentPhase, Option<u64>, Option<u64>)>,
    is_live: bool,
) -> SessionInfo {
    let empty = crate::db::DbSessionStats::default();
    let ds = db_stats.unwrap_or(&empty);

    let stats = SessionStats {
        user_messages: ds.user_messages,
        assistant_messages: ds.assistant_messages,
        tool_calls: ds.tool_calls,
        tool_results: ds.tool_results,
        tokens: TokenStats {
            input: ds.tokens_input,
            output: ds.tokens_output,
            cache_read: ds.tokens_cache_read,
            cache_write: ds.tokens_cache_write,
        },
        cost: ds.cost,
        is_subscription: stored.is_subscription,
        context_window: stored.model.context_window,
        context_tokens: ds.last_input_tokens,
    };

    let context_pct = if stats.context_window > 0 {
        stats
            .context_tokens
            .map(|t| (t as f64 / stats.context_window as f64) * 100.0)
    } else {
        None
    };

    let phase_value = phase.map(|p| p.0);
    let turn_started_at_ms = phase.and_then(|p| p.1);
    let phase_started_at_ms = phase.and_then(|p| p.2);
    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: ds.message_count,
        stats,
        last_activity: ds.last_message_time.unwrap_or(stored.created_at) / 1000,
        parent_id: stored.parent_id.clone(),
        child_count,
        child_budget: stored.child_budget,
        tagline: stored.tagline.clone(),
        state: phase_value
            .unwrap_or_default()
            .label()
            .trim_end_matches("...")
            .to_string(),
        context_pct,
        archived: stored.archived,
        last_exit_status: stored.last_exit_status.clone(),
        is_live,
        project_name: stored.project_name.clone(),
        successor_id: stored.successor_id.clone(),
        turn_started_at_ms,
        phase_started_at_ms,
    }
}

pub(super) fn model_info(m: &Model) -> ModelInfo {
    ModelInfo {
        id: m.id.clone(),
        name: m.name.clone(),
        provider: m.provider.clone(),
        thinking: m.thinking.clone(),
        context_window: m.context_window,
        max_tokens: m.max_tokens,
    }
}

/// Resolve API key: auth.json → config provider api_key → env var.
pub(super) fn resolve_api_key(
    auth: &crate::auth::AuthStorage,
    cfg: &crate::config::Config,
    provider: &str,
) -> crate::Result<Option<String>> {
    resolve_api_key_excluding(auth, cfg, provider, None)
}

/// Like `resolve_api_key` but tells the auth store which token just got
/// rejected by the server, so it can avoid re-issuing that same token and
/// can skip the OAuth refresh HTTP call when a sibling session has
/// already written a fresher one to `auth.json`.  See
/// `AuthStorage::get_api_key_excluding` for full semantics.
pub(super) fn resolve_api_key_excluding(
    auth: &crate::auth::AuthStorage,
    cfg: &crate::config::Config,
    provider: &str,
    stale: Option<&str>,
) -> crate::Result<Option<String>> {
    // First try auth storage (handles OAuth refresh, env vars, etc.)
    if let Ok(Some(key)) = auth.get_api_key_excluding(provider, stale) {
        return Ok(Some(key));
    }
    // Then try config's inline api_key
    if let Some(pc) = cfg.providers.get(provider)
        && let Some(key) = crate::config::resolve_provider_api_key(pc)
    {
        tracing::debug!(provider, "resolve_api_key: from config");
        return Ok(Some(key));
    }
    // Diagnostic: nothing produced a key. Capture which sources had
    // *any* presence of the provider so the post-hoc log narrows the
    // hypothesis space (auth race vs config typo vs env unset).
    let has_auth_entry = auth.get(provider).ok().flatten().is_some();
    let has_env = crate::auth::env_api_key_for_diagnostics(provider).is_some();
    let has_config = cfg.providers.get(provider).is_some();
    tracing::warn!(
        provider,
        has_auth_entry,
        has_env,
        has_config,
        "resolve_api_key: returning Ok(None) — caller will surface NoApiKey"
    );
    Ok(None)
}

/// Build registry with all known API providers.
pub(super) fn build_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(crate::providers::anthropic::Anthropic);
    registry.register(crate::providers::openai::OpenAi);
    registry.register(crate::providers::log::LogProvider);
    registry
}

/// Spawn a background task that periodically sends idle notifications
/// to session plugins that have no active subscribers.
pub(super) fn spawn_idle_sweep(
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    state: SharedState,
    shutdown: ShutdownHandle,
) {
    let idle_timeout = plugins
        .lock()
        .expect("plugins mutex poisoned")
        .idle_timeout();
    if idle_timeout.is_zero() {
        return; // Idle sweep disabled
    }
    // Sweep interval: half the idle timeout, minimum 5s
    let interval = std::cmp::max(idle_timeout / 2, std::time::Duration::from_secs(5));
    smol::spawn(async move {
        loop {
            smol::Timer::after(interval).await;
            if shutdown.is_shutting_down() {
                break;
            }
            // Collect subscriber info on the async side (cheap, just lock state briefly)
            let subscribed_sessions: std::collections::HashSet<String> = {
                let st = lock_state(&state);
                st.subscribers
                    .iter()
                    .filter(|(_, subs)| !subs.is_empty())
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            // Run sweep on a blocking thread since plugin I/O is synchronous
            let plugins_clone = plugins.clone();
            let _ = smol::unblock(move || {
                let mut pm = plugins_clone.lock().expect("plugins mutex poisoned");
                pm.idle_sweep(idle_timeout, &|session_id: &str| {
                    subscribed_sessions.contains(session_id)
                });
            })
            .await;
        }
    })
    .detach();
}

/// Read one `PluginMessage` from an async stdout reader.
pub(super) async fn read_plugin_message(
    reader: &mut crate::plugin::AsyncPluginReader,
) -> crate::Result<crate::plugin::PluginMessage> {
    crate::read_json_line_async(reader)
        .await
        .map_err(|e| crate::Error::Io(format!("read from plugin: {}", e)))?
        .ok_or_else(|| crate::Error::Io("plugin closed stdout".into()))
}

/// Write a `PluginRequest` to an async stdin writer.
pub(super) async fn write_plugin_request(
    writer: &mut crate::plugin::AsyncPluginWriter,
    req: &crate::plugin::PluginRequest,
) -> crate::Result<()> {
    crate::write_json_line_async(writer, req)
        .await
        .map_err(|e| crate::Error::Io(format!("write to plugin: {}", e)))
}

/// Create a chat-spawn channel with a receiver task that fires off
/// `run_child_chat` for each `ChatSpawn` message.
///
/// Used by `spawn_global_plugin_background_tasks` so that background
/// `ServerRequest::Chat` calls can spawn agent turns.
pub(super) fn spawn_bg_chat_receiver(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
) -> smol::channel::Sender<super::state::ChatSpawn> {
    let (tx, rx) = smol::channel::unbounded::<super::state::ChatSpawn>();
    smol::spawn(async move {
        while let Ok(spawn) = rx.recv().await {
            let s = state.clone();
            let p = plugins.clone();
            let sh = shutdown.clone();
            let sl = session_locks.clone();
            let th = throttle.clone();
            let ov: SharedTestOverrides = Arc::new(TestOverrides::default());
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
                    tracing::warn!(session_id = %sid, %e, "bg child chat error");
                }
            })
            .detach();
        }
    })
    .detach();
    tx
}

/// Spawn background reader/writer tasks for all global plugins.
///
/// For each global plugin:
/// - A **reader task** reads messages from the plugin's stdout.
///   `ServerRequest` messages are handled inline (via `handle_server_request`);
///   all other messages (e.g. `ToolResult`, `OutputDelta`) are forwarded to the
///   plugin handle through a channel so that `PluginExecutor` can consume them
///   during tool calls.
/// - A **writer task** drains a channel of `PluginRequest` messages and writes
///   them to the plugin's stdin.  Both the `PluginExecutor` (via `send_async`)
///   and the reader task (to send `ServerResponse`) share this channel.
///
/// These tasks are detached and run until the plugin dies or the server shuts
/// down.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_global_plugin_background_tasks(
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    state: &SharedState,
    session_locks: &SessionLocks,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    chat_spawn_tx: &smol::channel::Sender<super::state::ChatSpawn>,
    test_overrides: &SharedTestOverrides,
) {
    let io_pairs = {
        let mut pm = plugins.lock().expect("plugins mutex poisoned");
        pm.setup_background_io()
    };

    for (plugin_name, mut reader, mut writer, msg_tx, write_rx) in io_pairs {
        // --- Writer task: drain write_rx → stdin ---
        let writer_plugin_name = plugin_name.clone();
        smol::spawn(async move {
            while let Ok(req) = write_rx.recv().await {
                if let Err(e) = write_plugin_request(&mut writer, &req).await {
                    tracing::warn!(
                        plugin = %writer_plugin_name,
                        %e,
                        "global plugin background writer error"
                    );
                    break;
                }
            }
        })
        .detach();

        // --- Reader task: stdout → route messages ---
        let reader_state = state.clone();
        let reader_session_locks = session_locks.clone();
        let reader_plugins = plugins.clone();
        let reader_shutdown = shutdown.clone();
        let reader_throttle = throttle.clone();
        let reader_chat_tx = chat_spawn_tx.clone();
        let reader_test_overrides = test_overrides.clone();
        // Get a sender clone for the writer channel so the reader task can
        // send ServerResponse messages back to the plugin.
        let resp_tx = {
            let pm = plugins.lock().expect("plugins mutex poisoned");
            pm.get_global_write_tx(&plugin_name)
        };
        let resp_tx = match resp_tx {
            Some(tx) => tx,
            None => {
                tracing::warn!(
                    plugin = %plugin_name,
                    "global plugin: no write channel for background reader"
                );
                continue;
            }
        };

        smol::spawn(async move {
            loop {
                let msg = match read_plugin_message(&mut reader).await {
                    Ok(msg) => msg,
                    Err(e) => {
                        // Don't log during shutdown — plugin may have been killed.
                        if !reader_shutdown.is_shutting_down() {
                            tracing::warn!(
                                plugin = %plugin_name,
                                %e,
                                "global plugin background reader error"
                            );
                        }
                        break;
                    }
                };

                match msg {
                    crate::plugin::PluginMessage::ServerRequest {
                        request_id,
                        request,
                    } => {
                        let response = handle_server_request(
                            &reader_state,
                            &reader_session_locks,
                            &reader_plugins,
                            &reader_shutdown,
                            &reader_throttle,
                            &reader_chat_tx,
                            &reader_test_overrides,
                            &request,
                            // Background requests have no specific session context;
                            // use an empty session ID.
                            "",
                        )
                        .await;
                        let resp_req = crate::plugin::PluginRequest::ServerResponse {
                            request_id,
                            response,
                        };
                        if resp_tx.send(resp_req).await.is_err() {
                            tracing::warn!(
                                plugin = %plugin_name,
                                "global plugin background reader: write channel closed"
                            );
                            break;
                        }
                    }
                    other => {
                        // Forward to plugin handle for tool-call consumption.
                        if msg_tx.send(other).await.is_err() {
                            // Handle was dropped (plugin killed / reloaded).
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }
}

/// Check whether a session has pending queued messages and, if so, spawn a
/// `resume_child_session` task so they are processed.
pub(super) fn maybe_respawn_for_queued(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    session_id: &str,
    test_overrides: &SharedTestOverrides,
) {
    let has_pending = {
        let st = lock_state(state);
        st.has_queued
            .get(session_id)
            .map(|f| f.load(Ordering::Acquire))
            .unwrap_or(false)
    };
    if has_pending {
        let s = state.clone();
        let p = plugins.clone();
        let sh = shutdown.clone();
        let sl = session_locks.clone();
        let th = throttle.clone();
        let ov = test_overrides.clone();
        let sid = session_id.to_string();
        smol::spawn(async move {
            if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                tracing::warn!(session_id = %sid, %e, "resume session for late-queued message failed");
            }
        })
        .detach();
    }
}
