//! Subscriber notification primitives.
//!
//! # Broadcast delivery rules
//!
//! Subscribers are registered per-session as `smol::channel::Sender<Response>`
//! values. There are two broadcast primitives:
//!
//! * [`broadcast_to_subscribers`] — fire-and-forget `try_send`.  If a
//!   subscriber's channel is full, the message is dropped (with a log line)
//!   and the subscriber is retained. This is the right shape for
//!   high-frequency streaming deltas (`TextDelta`, `ThinkingDelta`, transient
//!   `Phase` transitions, `Status` updates) where backpressure would slow the
//!   entire agent loop and stale drops are acceptable.
//!
//! * [`broadcast_to_subscribers_and_wait`] — async `send().await` on each
//!   subscriber. Guarantees the message is enqueued in every subscriber's
//!   channel before returning, so later ordering-sensitive events (or the
//!   session becoming idle from a different code path) can't race ahead of
//!   this one. Used for terminal responses where ordering matters:
//!   `AgentDone`, terminal `Cancelled`, terminal `Phase(Idle)`, and
//!   shutdown-path broadcasts.
//!
//! **Rule of thumb**: if dropping or reordering the message could leave a
//! subscriber (TUI, API client, parent session) in a stale state, use the
//! awaiting variant. Otherwise the fast path is fine.
//!
//! The awaiting variant deliberately does **not** hold the state mutex across
//! `.await`: it clones the subscriber senders out of the map, drops the
//! state lock, and then awaits each send. This avoids a slow subscriber
//! blocking unrelated session traffic.

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
        tracing::warn!(%e, "failed to persist info message");
    }
}

/// Return true iff the target session exists and is a "no-agent-loop"
/// placeholder session — i.e. its provider is registered and reports
/// `needs_api_key() == false` (today: the built-in `log` provider).
///
/// Such sessions are append-only audit logs, not interlocutors.
/// Callers use this to short-circuit `Chat` / `QueueMessage` requests
/// targeting a placeholder: the message is recorded to history but no
/// agent turn is run. See task 582.
pub(super) fn is_no_agent_loop_session(state: &SharedState, target: &str) -> bool {
    let st = lock_state(state);
    let Ok(Some(session)) = st.db.get_session(target) else {
        return false;
    };
    !st.registry.needs_api_key(&session.model.api)
}

/// Human-readable note emitted when a message is accepted by a placeholder
/// (log-provider) session without triggering an agent turn. Surfaced to
/// `QueueMessage`/`Chat` callers so they understand why no reply arrived.
pub(super) fn placeholder_no_agent_note(target: &str) -> String {
    format!(
        "Message recorded on session {sid}. Note: {sid} is a log-only \
         session (placeholder) \u{2014} it does not run an agent loop, so \
         this message is appended to the session's history but no \
         response will be generated.",
        sid = target
    )
}

/// Record a message onto a log-provider / placeholder session without
/// running the agent loop. See `is_no_agent_loop_session` for context.
///
/// Behaviour:
///   * Appends the message as a `Message::User` to the session's history.
///   * Does NOT insert into `queued_messages` — there's no resume path.
///   * Does NOT call the agent runner.
///   * Does NOT emit phase transitions.
///
/// Intentionally mirrors `queue_info_to_session` but uses a `User` message
/// so the content is preserved as the sender wrote it (not display-only),
/// matching the "placeholder == append-only audit log" semantic.
pub(super) fn record_message_to_log_session(state: &SharedState, target: &str, content: &str) {
    let user_msg = Message::User(UserMessage::text(content));
    let st = lock_state(state);
    if let Err(e) = st.db.append_message(target, &user_msg) {
        tracing::warn!(session_id = %target, %e, "failed to persist message to log session");
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
                tracing::warn!(session_id = %sid, %e, "resume session after queued message failed");
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
///
/// Uses fire-and-forget broadcast — suitable for transient phase transitions
/// (Thinking, Responding, ToolExec, ...). For the terminal `Idle` transition
/// at the end of a run, prefer [`emit_phase_and_wait`] so the TUI can't
/// observe idleness before receiving the final `AgentDone`.
pub(super) fn emit_phase(state: &SharedState, session_id: &str, phase: crate::types::AgentPhase) {
    let resp = persist_phase(state, session_id, phase);
    broadcast_to_subscribers(state, session_id, &resp);
}

/// Awaiting variant of [`emit_phase`]. Ensures every subscriber has enqueued
/// the Phase event before returning. Use for terminal `Phase(Idle)`.
pub(super) async fn emit_phase_and_wait(
    state: &SharedState,
    session_id: &str,
    phase: crate::types::AgentPhase,
) {
    let resp = persist_phase(state, session_id, phase);
    broadcast_to_subscribers_and_wait(state, session_id, &resp).await;
}

fn persist_phase(
    state: &SharedState,
    session_id: &str,
    phase: crate::types::AgentPhase,
) -> Response {
    let (turn_started_at_ms, phase_started_at_ms) = {
        let mut st = lock_state(state);
        let ts = set_phase_and_stamp_locked(&mut st, session_id, phase);
        // Persist meaningful phase transitions to DB.
        let label = phase.label().trim_end_matches("...");
        if let Err(e) = st.db.update_phase(session_id, label) {
            tracing::warn!(session_id = %session_id, %e, "failed to persist phase");
        }
        ts
    };
    Response::Stream {
        event: Box::new(crate::types::StreamEvent::Phase {
            phase,
            turn_started_at_ms,
            phase_started_at_ms,
        }),
    }
}

/// Update the in-memory phase map and return the turn-start and
/// phase-start anchors for the resulting phase.
///
/// Rules:
/// * Transitioning to `Idle` clears any existing anchors and returns
///   `(None, None)`.
/// * Transitioning to a non-Idle phase stamps `turn_start = Some(now_ms)`
///   *only if* there is no existing turn anchor for this session (i.e.
///   we are starting a fresh turn). Subsequent non-Idle transitions
///   preserve the existing turn anchor so the counter is continuous for
///   the whole turn.
/// * `phase_start` is re-stamped to `Some(now_ms)` on every real phase
///   transition (`old_phase != new_phase`). On a defensive same-phase
///   call (incoming phase equals the stored phase) we preserve the
///   existing `phase_start` so repeated implicit-phase events within
///   the same phase don't reset the counter.
///
/// Callers: `persist_phase` above, and the stream-event forward loop in
/// `agent_runner.rs` that derives implicit phases from text/tool events.
pub(super) fn set_phase_and_stamp_locked(
    st: &mut super::state::State,
    session_id: &str,
    phase: crate::types::AgentPhase,
) -> (Option<u64>, Option<u64>) {
    let now_ms = crate::types::timestamp_ms();
    match phase {
        crate::types::AgentPhase::Idle => {
            st.phases
                .insert(session_id.to_string(), (phase, None, None));
            (None, None)
        }
        _ => {
            let entry = st.phases.entry(session_id.to_string()).or_insert((
                phase,
                Some(now_ms),
                Some(now_ms),
            ));
            // Preserve existing turn anchor on phase→phase transitions;
            // stamp only if there was no anchor (i.e. session was Idle
            // / unknown).
            if entry.1.is_none() {
                entry.1 = Some(now_ms);
            }
            // Re-stamp phase anchor on real transitions; preserve on
            // same-phase (defensive: implicit-phase events that don't
            // change the phase shouldn't reset the per-phase counter).
            if entry.0 != phase || entry.2.is_none() {
                entry.2 = Some(now_ms);
            }
            entry.0 = phase;
            (entry.1, entry.2)
        }
    }
}

/// Variant of [`set_phase_and_stamp_locked`] that takes a `SharedState` and
/// manages the lock internally. Returns the turn-start and phase-start
/// anchors for the resulting phase. Used by the stream-forward loop that
/// otherwise only holds the lock for the mutation.
pub(super) fn set_phase_and_stamp(
    state: &SharedState,
    session_id: &str,
    phase: crate::types::AgentPhase,
) -> (Option<u64>, Option<u64>) {
    let mut st = lock_state(state);
    set_phase_and_stamp_locked(&mut st, session_id, phase)
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

/// Fire-and-forget broadcast to subscribers (`try_send`).
/// If a subscriber's channel is full, the message is dropped. See the
/// module-level comment for when to use this vs. [`broadcast_to_subscribers_and_wait`].
pub(super) fn broadcast_to_subscribers(state: &SharedState, session_id: &str, resp: &Response) {
    let mut st = lock_state(state);
    if let Some(subs) = st.subscribers.get_mut(session_id) {
        subs.retain(|tx| {
            match tx.try_send(resp.clone()) {
                Ok(()) => true,
                Err(smol::channel::TrySendError::Closed(_)) => false,
                Err(smol::channel::TrySendError::Full(_)) => {
                    tracing::warn!("subscriber channel full, dropping message");
                    true // keep subscriber, just drop this message
                }
            }
        });
        if subs.is_empty() {
            st.subscribers.remove(session_id);
        }
    }
}

/// Awaiting broadcast to subscribers. Uses `send().await` on each subscriber
/// so a bounded/full channel backpressures the emitter until the subscriber
/// has made room. Guarantees every live subscriber has the message enqueued
/// (in subscription order) before this function returns.
///
/// Does **not** hold the state lock across `.await`: clones subscriber
/// senders out of the map, drops the lock, then awaits each send. Closed
/// subscribers are pruned from the map after delivery.
pub(super) async fn broadcast_to_subscribers_and_wait(
    state: &SharedState,
    session_id: &str,
    resp: &Response,
) {
    // Clone subscriber senders out under the lock, then drop it.
    let senders: Vec<smol::channel::Sender<Response>> = {
        let st = lock_state(state);
        match st.subscribers.get(session_id) {
            Some(subs) => subs.clone(),
            None => return,
        }
    };

    // Await delivery to each subscriber in registration order. Track which
    // senders dropped so we can prune them afterwards.
    let mut closed_indices: Vec<usize> = Vec::new();
    for (idx, tx) in senders.iter().enumerate() {
        if let Err(smol::channel::SendError(_)) = tx.send(resp.clone()).await {
            // Receiver dropped — the subscriber disconnected.
            closed_indices.push(idx);
        }
    }

    if closed_indices.is_empty() {
        return;
    }

    // Prune closed subscribers. We match on Sender equality via `same_channel`
    // to avoid removing senders that were newly added while we awaited.
    let mut st = lock_state(state);
    if let Some(subs) = st.subscribers.get_mut(session_id) {
        let closed: Vec<&smol::channel::Sender<Response>> =
            closed_indices.iter().map(|&i| &senders[i]).collect();
        subs.retain(|tx| !closed.iter().any(|c| c.same_channel(tx)));
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
            tracing::warn!(session_id = %sid, %e, "auto-archive session failed");
        }
    }
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
    use std::time::Duration;

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

    /// A bounded-buffer subscriber should backpressure `broadcast_to_subscribers_and_wait`
    /// until its receiver reads. This guards the ordering guarantee that
    /// motivated this module (TUI must observe `AgentDone` before idleness).
    #[test]
    fn awaiting_broadcast_backpressures_full_subscriber() {
        smol::block_on(async {
            let state = mk_state();
            let sid = "s-test";

            // Register a bounded(1) subscriber. Fill it so the next send must block.
            let (tx, rx) = smol::channel::bounded::<Response>(1);
            {
                let mut st = lock_state(&state);
                st.subscribers.insert(sid.into(), vec![tx]);
            }

            // Occupy the 1-slot buffer with a dummy value so AgentDone must wait.
            {
                let st = lock_state(&state);
                let subs = st.subscribers.get(sid).expect("subs present");
                subs[0]
                    .try_send(Response::Ok)
                    .expect("room for one pre-filled value");
            }

            // Race the awaiting broadcast against a timeout. It should *not*
            // complete until the receiver drains the channel.
            let broadcast = broadcast_to_subscribers_and_wait(&state, sid, &Response::AgentDone);
            futures::pin_mut!(broadcast);
            let timeout = smol::Timer::after(Duration::from_millis(100));
            futures::pin_mut!(timeout);
            let result = futures::future::select(broadcast, timeout).await;
            assert!(
                matches!(result, futures::future::Either::Right(_)),
                "awaiting broadcast completed before subscriber drained"
            );

            // Receiver now reads the pre-filled dummy, freeing a slot.
            assert!(matches!(rx.recv().await, Ok(Response::Ok)));

            // The still-pending broadcast future should resolve quickly now.
            let broadcast = match result {
                futures::future::Either::Left(_) => unreachable!(),
                futures::future::Either::Right((_, fut)) => fut,
            };
            let done = smol::Timer::after(Duration::from_secs(2));
            futures::pin_mut!(done);
            let result2 = futures::future::select(broadcast, done).await;
            assert!(
                matches!(result2, futures::future::Either::Left(_)),
                "awaiting broadcast did not settle after subscriber drained"
            );

            // AgentDone is now readable on the subscriber.
            assert!(matches!(rx.recv().await, Ok(Response::AgentDone)));
        });
    }

    /// Two consecutive awaited broadcasts of `AgentDone` are both delivered
    /// in order, with no coalescing or drops.
    #[test]
    fn two_consecutive_agent_done_events_are_both_delivered() {
        smol::block_on(async {
            let state = mk_state();
            let sid = "s-test";
            let (tx, rx) = smol::channel::bounded::<Response>(4);
            {
                let mut st = lock_state(&state);
                st.subscribers.insert(sid.into(), vec![tx]);
            }

            broadcast_to_subscribers_and_wait(&state, sid, &Response::AgentDone).await;
            broadcast_to_subscribers_and_wait(&state, sid, &Response::AgentDone).await;

            assert!(matches!(rx.recv().await, Ok(Response::AgentDone)));
            assert!(matches!(rx.recv().await, Ok(Response::AgentDone)));
        });
    }

    /// A closed subscriber (receiver dropped) is pruned by the awaiting
    /// broadcast and does not prevent delivery to other subscribers.
    #[test]
    fn awaiting_broadcast_prunes_closed_subscriber() {
        smol::block_on(async {
            let state = mk_state();
            let sid = "s-test";

            let (tx_closed, rx_closed) = smol::channel::bounded::<Response>(1);
            let (tx_live, rx_live) = smol::channel::bounded::<Response>(1);
            {
                let mut st = lock_state(&state);
                st.subscribers.insert(sid.into(), vec![tx_closed, tx_live]);
            }

            // Close the first subscriber by dropping its receiver.
            drop(rx_closed);

            broadcast_to_subscribers_and_wait(&state, sid, &Response::AgentDone).await;

            // The live subscriber received the message.
            assert!(matches!(rx_live.recv().await, Ok(Response::AgentDone)));

            // The closed subscriber was pruned; only the live sender remains.
            let st = lock_state(&state);
            let subs = st.subscribers.get(sid).expect("subs present");
            assert_eq!(subs.len(), 1, "closed subscriber should be pruned");
        });
    }

    /// The fire-and-forget variant drops the message if the channel is full
    /// but keeps the subscriber (historical behaviour, preserved for
    /// high-frequency streaming deltas).
    #[test]
    fn fire_and_forget_drops_when_full_but_keeps_subscriber() {
        let state = mk_state();
        let sid = "s-test";
        let (tx, rx) = smol::channel::bounded::<Response>(1);
        {
            let mut st = lock_state(&state);
            st.subscribers.insert(sid.into(), vec![tx]);
        }
        // Fill the channel.
        let st = lock_state(&state);
        st.subscribers[sid][0].try_send(Response::Ok).unwrap();
        drop(st);

        // Best-effort broadcast drops the second message silently.
        broadcast_to_subscribers(&state, sid, &Response::AgentDone);

        // Subscriber is still registered.
        let st = lock_state(&state);
        assert_eq!(st.subscribers[sid].len(), 1);
        drop(st);

        // Only the first message is present.
        assert!(matches!(rx.try_recv(), Ok(Response::Ok)));
        assert!(rx.try_recv().is_err());
    }

    /// Task 702: `set_phase_and_stamp_locked` stamps a fresh
    /// `phase_started_at_ms` on every real phase transition while
    /// preserving `turn_started_at_ms` across the same turn.
    #[test]
    fn set_phase_and_stamp_stamps_phase_start_on_transition() {
        let state = mk_state();
        let sid = "s-test";
        // Idle → Thinking: both anchors stamped to ~now.
        let (turn1, phase1) = {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::Thinking)
        };
        let turn1 = turn1.expect("turn anchor stamped on Idle→Thinking");
        let phase1 = phase1.expect("phase anchor stamped on Idle→Thinking");
        assert!(
            phase1 >= turn1,
            "phase anchor must be stamped no earlier than turn anchor"
        );

        // Pause briefly so timestamps move.
        std::thread::sleep(Duration::from_millis(5));

        // Thinking → Responding: turn anchor preserved, phase anchor advances.
        let (turn2, phase2) = {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::Responding)
        };
        assert_eq!(
            turn2,
            Some(turn1),
            "turn anchor must persist across phase→phase transitions"
        );
        let phase2 = phase2.expect("phase anchor still set");
        assert!(
            phase2 > phase1,
            "phase anchor must advance on real phase transition: phase2={phase2} phase1={phase1}"
        );
    }

    /// Task 702: same-phase calls (defensive: shouldn't happen but the
    /// stream-event forward loop may funnel implicit phase events that
    /// don't change the phase) preserve the existing `phase_started_at_ms`.
    #[test]
    fn set_phase_and_stamp_preserves_phase_start_within_phase() {
        let state = mk_state();
        let sid = "s-test";
        let (_, phase1) = {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::ToolExec)
        };
        let phase1 = phase1.expect("phase anchor stamped");

        std::thread::sleep(Duration::from_millis(5));

        // Same phase again — phase anchor must NOT advance.
        let (_, phase2) = {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::ToolExec)
        };
        assert_eq!(
            phase2,
            Some(phase1),
            "phase anchor must be preserved across same-phase calls"
        );
    }

    /// Task 702: any non-Idle → Idle transition clears both anchors and
    /// returns `(None, None)`.
    #[test]
    fn set_phase_and_stamp_idle_clears_both() {
        let state = mk_state();
        let sid = "s-test";
        {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::Responding);
        }
        let (turn, phase) = {
            let mut st = lock_state(&state);
            set_phase_and_stamp_locked(&mut st, sid, crate::types::AgentPhase::Idle)
        };
        assert_eq!(turn, None, "Idle clears turn anchor");
        assert_eq!(phase, None, "Idle clears phase anchor");
        // Stored entry is `(Idle, None, None)`.
        let st = lock_state(&state);
        let entry = st.phases.get(sid).copied().expect("entry stored");
        assert!(matches!(entry.0, crate::types::AgentPhase::Idle));
        assert_eq!(entry.1, None);
        assert_eq!(entry.2, None);
    }
}
