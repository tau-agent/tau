//! JSON-lines wire protocol over unix domain socket.

use serde::{Deserialize, Serialize};

use crate::subscription_usage::{SubscriptionUsage, UsageBucket};
use crate::types::StreamEvent;

// ---------------------------------------------------------------------------
// Client → Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Send a chat message in a session.
    Chat {
        session_id: String,
        text: String,
        /// Optional attachments (images for now). Empty by default for
        /// backward compatibility with older clients/payloads.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<ChatAttachment>,
    },
    /// Create a new session.
    CreateSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        /// Working directory for tool execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Parent session ID (for child sessions).
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Max descendant sessions this session can spawn.
        #[serde(default)]
        child_budget: u32,
        /// Short description of the session's task.
        #[serde(skip_serializing_if = "Option::is_none")]
        tagline: Option<String>,
        /// When true, auto-archive this session after completion+join.
        #[serde(default)]
        auto_archive: bool,
        /// When true, notify parent session on child completion (default true).
        #[serde(default = "default_true")]
        notify_parent: bool,
        /// Project name (from discover_project or explicit).
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
        /// Sandbox profile name (from task config) for plugin spawning.
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox_profile: Option<String>,
    },
    /// Get info about a specific session.
    GetSessionInfo { session_id: String },
    /// Return the requested session and all its ancestors.
    ///
    /// Ordered leaf-first: index 0 is `session_id`, the last entry is the root
    /// (or the deepest reachable ancestor when the depth guard trips or a
    /// `parent_id` points at a missing row).
    ///
    /// Returns an empty `sessions` vec if `session_id` itself is unknown —
    /// **not** an error response.
    GetSessionAncestors { session_id: String },
    /// List sessions.
    ListSessions {
        /// Include archived sessions in the listing.
        #[serde(default)]
        include_archived: bool,
        /// If set, only list sessions belonging to this project.
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
    },
    /// Archive a session (and all its children).
    ArchiveSession {
        session_id: String,
        /// If set, the server verifies that `session_id` is a descendant of
        /// this ancestor before archiving.  The TUI sends `None` (no
        /// restriction); orchestration tools send `Some(current_session_id)`.
        #[serde(default)]
        require_ancestor: Option<String>,
    },
    /// Restore (un-archive) a session and all its descendants.
    RestoreSession { session_id: String },
    /// Delete a session.
    DeleteSession { session_id: String },
    /// List available models.
    ListModels,
    /// List configured aliases (global + per-project).
    ///
    /// `cwd` is the project directory whose `.tau/models.toml` should be
    /// inspected for project-level aliases.  Pass `None` to get global
    /// aliases only.
    ///
    /// Added in protocol v0.2: older servers will respond with an error.
    /// Clients should treat that as "no aliases" and degrade gracefully.
    ListAliases {
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Change model for a session.
    SetModel {
        session_id: String,
        model_id: String,
        /// Session id of the caller when invoked via an orchestration tool
        /// (used to attribute the change in the session's info-message log).
        /// `None` when invoked by the TUI/CLI/external API.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_session_id: Option<String>,
    },
    /// Change working directory for a session.
    SetCwd {
        session_id: String,
        cwd: String,
        /// Session id of the caller when invoked via an orchestration tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_session_id: Option<String>,
    },
    /// Re-parent all child sessions from one parent to another.
    ReparentChildren {
        old_parent_id: String,
        new_parent_id: String,
    },
    /// Mark `session_id` as superseded by `successor_id`. Future
    /// notifications / queued messages / new-child-parent-anchor lookups
    /// targeted at `session_id` are forwarded to the resolved tip of the
    /// successor chain.  `successor_id == None` clears the link (un-retire).
    ///
    /// The predecessor stays in the DB — message history is preserved
    /// and remains readable — but it will not receive new wakeups while
    /// a successor is set.  See task 914.
    SetSessionSuccessor {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        successor_id: Option<String>,
        /// Session id of the caller when invoked via an orchestration tool.
        /// `None` when invoked by the TUI/CLI/external API.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_session_id: Option<String>,
    },
    /// Resolve the live tip of `session_id`'s successor chain.
    ///
    /// Returns [`Response::ResolvedSuccessor`].  Used by plugins (notably
    /// the tasks plugin) to redirect notifications away from retired
    /// sessions.  See task 914.
    ResolveSuccessor { session_id: String },
    /// Atomically create a new session inheriting `session_id`'s
    /// `model` / `cwd` / `system_prompt` / `project_name` / `child_budget`,
    /// then mark `session_id` as retired by setting its `successor_id`
    /// to the new session's id.
    ///
    /// The new session is always **top-level** (`parent_id = None`) so
    /// succession does not change the predecessor's place in the session
    /// tree.  Returns [`Response::SessionCreated`] with the successor id
    /// on success and broadcasts [`Response::SessionSucceeded`] on the
    /// predecessor's subscriber channel.  See task 915.
    SucceedSession {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tagline: Option<String>,
        /// Session id of the caller when invoked via an orchestration tool
        /// (e.g. `session_succeed`).  `None` when invoked via the TUI/CLI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_session_id: Option<String>,
    },
    /// Look up whether `session_id` is recorded as a session of any
    /// non-terminal task and, if so, return the `(task_id, role)` it
    /// plays.  Used by orchestration tools (today: `session_succeed`)
    /// that must refuse to disturb a task-managed session lifecycle.
    ///
    /// Returns [`Response::TaskSessionRole`] always — a non-task session
    /// yields `is_worker = false` with `task_id` / `role` set to `None`.
    /// See task 915.
    GetTaskSessionRole { session_id: String },
    /// Start OAuth login for a provider.
    Login { provider: String },
    /// Query authentication status.
    AuthStatus,
    /// Fetch subscription usage (OAuth only, cached 5 min).
    GetSubscriptionUsage,
    /// Get message history for a session.
    GetMessages { session_id: String },
    /// Subscribe to live events on a session (for multi-client).
    /// The connection stays open and receives Stream/AgentDone/Cancelled events.
    Subscribe { session_id: String },
    /// Wait for sessions to complete.
    WaitSessions {
        session_ids: Vec<String>,
        #[serde(default = "default_wait_timeout")]
        timeout_secs: u64,
    },
    /// Wait for any of the specified sessions to complete (returns as soon as >= 1 is done).
    WaitAnySessions {
        session_ids: Vec<String>,
        #[serde(default = "default_wait_timeout")]
        timeout_secs: u64,
    },
    /// Cancel an in-progress chat (agent loop) for a session.
    CancelChat {
        session_id: String,
        /// Session id of the caller when invoked via an orchestration tool
        /// (e.g. `session_cancel` from another session). `None` when invoked
        /// via the TUI/CLI/external API.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_session_id: Option<String>,
    },
    /// Inject a steering message into a running agent loop.
    /// The message is inserted as a user message between tool results
    /// and the next LLM call. If no agent is running, treated as Chat.
    Steer { session_id: String, text: String },
    /// Trigger context compaction now. Optional `keep_hint` is free-form
    /// text the summarizer is asked to preserve in addition to its standard
    /// sections (advisory, not a hard filter).
    Compact {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keep_hint: Option<String>,
    },

    /// Queue a message for delivery to a target session.
    /// When `await_reply` is true the caller blocks until the target
    /// calls `session_reply` with the corresponding `msg_id`.
    QueueMessage {
        target_session_id: String,
        content: String,
        sender_info: String,
        /// When true, block until the target replies.
        #[serde(default)]
        await_reply: bool,
        /// For threaded replies: the msg_id this message is responding to.
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    /// Persist a zero-token display-only info message to a session's
    /// message history. Unlike `QueueMessage`, this does **not** wake the
    /// agent loop and the message is excluded from LLM context.
    ///
    /// Intended for observational notifications such as task state-change
    /// info-lines surfaced in the TUI.
    QueueInfo {
        target_session_id: String,
        text: String,
    },
    /// Reply to a pending `await_reply` message.
    ReplyToMessage { msg_id: String, content: String },
    /// Reload plugins for a session (destroy + re-init).
    ReloadPlugins { session_id: String },
    /// Re-read `providers.toml` and global `models.toml` without restarting
    /// the server. On success, the in-memory provider/model tables and the
    /// global alias map are swapped in; on error (IO / parse failure) the
    /// existing state is left untouched and the server returns
    /// [`Response::Error`] so a broken edit can't brick a running server.
    ///
    /// Narrow by design: this does **not** reload plugins (see
    /// [`Request::ReloadPlugins`]), `auth.json` (re-read per request),
    /// or per-project `.tau/models.toml` (re-read per lookup).
    ReloadConfig,
    /// Garbage-collect archived sessions older than a threshold.
    GcSessions {
        /// Delete archived sessions older than this many days.
        older_than_days: u64,
    },
    /// Broadcast a hook to other plugins (plugin-to-plugin communication).
    FireHook {
        name: String,
        data: serde_json::Value,
    },
    /// Execute a tool directly on a session (no LLM involved).
    ExecuteTool {
        session_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// Enqueue a Tier-3 post-idle action for the given session. The server
    /// drains the queue once the session's lock releases (after the agent
    /// loop exits). Intended for side effects that need exclusive access
    /// to the caller's session or its subtree (e.g. archival, merge pass).
    ///
    /// See [`crate::types::PostIdleAction`] for the action semantics.
    EnqueuePostIdleAction {
        session_id: String,
        action: crate::types::PostIdleAction,
    },
    /// Set the tagline for a session.
    SetTagline { session_id: String, tagline: String },
    /// List tasks for a project.
    TaskList {
        project: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<i64>,
    },
    /// Get full details of a task.
    TaskGet { id: i64 },
    /// Create a new task.
    TaskCreate {
        project: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        priority: Option<i32>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox_profile: Option<String>,
    },
    /// Update a task (state, title, priority, etc.).
    TaskUpdate {
        id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        priority: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        affected_files: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skip_review: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        require_approval: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox_profile: Option<String>,
    },
    /// Search tasks by query.
    TaskSearch {
        project: String,
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },
    /// Assign a task to a session.
    TaskAssign { id: i64, session_id: String },
    /// Get scheduler status.
    TaskStatus { project: String },
    /// Structured task overview for interactive rendering.
    ///
    /// Returns active/queued/blocked/held tasks plus a bounded tail of
    /// recently-terminated (`merged` / `closed`) tasks, all as `TaskInfo`
    /// rather than pre-formatted text.  Consumers (the TUI task picker)
    /// render the overview grouped by scheduler position.
    ///
    /// `recent_limit` applies **per bucket** — up to `recent_limit` merged
    /// tasks **plus** up to `recent_limit` closed tasks, so the tail length
    /// is at most `2 * recent_limit`.
    TaskOverview {
        project: String,
        /// Max number of recently-terminated tasks to include *per bucket*
        /// (merged and closed are capped separately).  Defaults to 10.
        #[serde(default = "default_recent_limit")]
        recent_limit: usize,
    },
    /// Get merge queue (approved + merging tasks).
    TaskMergeQueue { project: String },
    /// Project-wide aggregate usage / cost stats.
    ///
    /// Returns totals across every session (archived included) belonging
    /// to `project_name`.
    ProjectStats { project_name: String },
    /// Look up a project by name. Returns the project's root path so the
    /// caller can recover when a session's `cwd` has disappeared
    /// (worktree removed, etc.) and the worker wants to fall back to the
    /// project root before executing a bash command. See task 720.
    GetProjectInfo { project_name: String },
    /// Shut down the server.
    Shutdown {
        /// If true, server is restarting (clients should reconnect).
        #[serde(default)]
        restart: bool,
    },
}

/// Attachments to a `Request::Chat` message.
///
/// Today only images are supported; the structure is an open enum so we can
/// add more attachment kinds without bumping the protocol shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatAttachment {
    /// An image. `data` is base64-encoded image bytes; `mime_type` is one
    /// of the MIME types accepted by the validator (`image/png`, `image/jpeg`,
    /// `image/gif`, `image/webp`). The server validates both fields before
    /// building the `UserMessage` and rejects oversized or malformed payloads
    /// with a `Response::Error` rather than panicking.
    Image { data: String, mime_type: String },
}

impl ChatAttachment {
    /// Convert this attachment into an engine `UserContent` block.
    ///
    /// Pure structural mapping; callers that need validation (decoded byte
    /// length, allowed MIME, etc.) should run that *before* calling this.
    pub fn to_user_content(&self) -> crate::types::UserContent {
        match self {
            ChatAttachment::Image { data, mime_type } => {
                crate::types::UserContent::Image(crate::types::ImageContent {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server → Client
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Session was created.
    SessionCreated { session_id: String },
    /// Info about a single session.
    SessionInfo { info: SessionInfo },
    /// Ancestor chain for a session, leaf-first.  See `Request::GetSessionAncestors`.
    SessionAncestors { sessions: Vec<SessionInfo> },
    /// List of sessions.
    Sessions { sessions: Vec<SessionInfo> },
    /// Session deleted.
    SessionDeleted,
    /// Session archived.
    SessionArchived,
    /// Session restored (un-archived).
    SessionRestored,
    /// Available models.
    Models { models: Vec<ModelInfo> },
    /// Configured aliases (global + per-project).
    ///
    /// Added in protocol v0.2.  Older clients will not understand this
    /// variant and will fall through to their default error path.
    Aliases {
        #[serde(default)]
        global: Vec<AliasInfo>,
        #[serde(default)]
        project: Vec<AliasInfo>,
    },
    /// Model changed.
    ModelChanged { model: ModelInfo },
    /// Streaming event from the LLM.
    Stream { event: Box<StreamEvent> },
    /// OAuth login succeeded.
    LoginSuccess { provider: String },
    /// Authentication status.
    AuthStatus { providers: Vec<String> },
    /// Subscription usage data.
    SubscriptionUsage { usage: SubscriptionUsage },
    /// Server is shutting down. Clients should reconnect if restart=true.
    ServerShutdown { restart: bool },
    /// Sessions completed (response to WaitSessions).
    SessionsCompleted { results: Vec<SessionResult> },
    /// Agent loop was cancelled by the user.
    Cancelled,
    /// Message history for a session.
    Messages {
        messages: Vec<crate::types::Message>,
    },
    /// A user message was sent (broadcast to subscribers).
    UserMessage { text: String },
    /// Agent loop completed (all turns done).
    AgentDone,
    /// Reply content (returned to a QueueMessage with await_reply=true).
    MessageReply { content: String },
    /// Success (generic ack).
    Ok,
    /// Success, with an advisory note from the server.
    ///
    /// Emitted in place of `Ok` when the server wants to tell the caller
    /// something about how the request was handled without treating it as
    /// an error. Today: `QueueMessage` (fire-and-forget) targeting a
    /// placeholder (log-provider) session — the note explains that the
    /// message was recorded but no agent loop ran. See task 582.
    ///
    /// Older clients that don't know this variant will fall through to
    /// their default-error path; the request still succeeded on the
    /// server side.
    OkWithNote { note: String },
    /// Garbage-collection result.
    GcComplete { deleted: usize },
    /// Tool execution result (response to ExecuteTool).
    ToolExecuted { content: String, is_error: bool },
    /// List of tasks (flat, for search/merge queue results).
    TaskList { tasks: Vec<TaskInfo> },
    /// Full task details (response to TaskGet).
    TaskDetail {
        task: TaskInfo,
        messages: Vec<TaskMessageInfo>,
        relations: Vec<TaskRelationInfo>,
        subtasks: Vec<TaskInfo>,
        /// Every `(session_id, role)` pair recorded for this task, enriched
        /// with best-effort session state.  Missing / deleted / cross-project
        /// sessions are dropped silently.  Back-compat: older clients that
        /// don't know about this field ignore it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sessions: Vec<TaskSessionInfo>,
        /// State transitions and other task updates in chronological order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        history: Vec<TaskHistoryInfo>,
    },
    /// Task created or updated (response to TaskCreate, TaskUpdate, TaskAssign).
    TaskUpdated { task: TaskInfo },
    /// Scheduler status text (response to TaskStatus).
    TaskStatus { text: String },
    /// Structured scheduler overview (response to TaskOverview).
    TaskOverview {
        /// Tasks in active lifecycle states (active, review, merging, refining).
        active: Vec<TaskInfo>,
        /// Tasks ready to dispatch (state=ready, not held, deps satisfied).
        queued_ready: Vec<TaskInfo>,
        /// Tasks queued for planning (state=planning, deps satisfied).
        queued_planning: Vec<TaskInfo>,
        /// Tasks blocked by unmet dependencies (state=ready or planning).
        blocked: Vec<TaskInfo>,
        /// Tasks held (state=ready or planning, held=true).
        held: Vec<TaskInfo>,
        /// Most recently merged tasks, newest first, capped at `recent_limit`
        /// (the request's per-bucket limit).
        recently_merged: Vec<TaskInfo>,
        /// Most recently closed tasks, newest first, capped at `recent_limit`
        /// (the request's per-bucket limit; merged and closed are independent).
        recently_closed: Vec<TaskInfo>,
        /// Current in-flight count (active/review/merging/refining).
        inflight_count: usize,
        /// Scheduler's max-concurrent budget.
        max_concurrent: usize,
        /// For each queued/blocked task id, the full list of wait reasons
        /// keeping it from dispatch. Dependency reasons drive the inline
        /// `⏳ #N` suffix in the picker; the detail pane renders every
        /// reason verbatim.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wait_reasons: Vec<TaskWaitReasons>,
    },
    /// Task list with tree depth info (response to TaskList).
    TaskTree { tasks: Vec<(usize, TaskInfo)> },
    /// Merge queue (approved + merging tasks, response to TaskMergeQueue).
    TaskMergeQueue { tasks: Vec<TaskInfo> },
    /// Project-wide usage / cost totals (response to `ProjectStats`).
    ProjectStats { stats: ProjectStatsInfo },
    /// Project metadata (response to `GetProjectInfo`).
    ///
    /// `project` is `None` when the named project does not exist; this is
    /// not treated as an error response so callers can match on "unknown
    /// project" cleanly.
    ProjectInfo { project: Option<ProjectInfoEntry> },
    /// Resolved tip of a session's successor chain (response to
    /// `Request::ResolveSuccessor`).  Returns the input session id
    /// unchanged when no successor is set or the chain dead-ends at an
    /// archived / missing successor.  See task 914.
    ResolvedSuccessor { session_id: String },
    /// Broadcast on the predecessor's subscriber channel when the session
    /// has been retired in favour of `successor_id`.  Subscribed clients
    /// (e.g. the TUI) typically react by switching their view to the
    /// successor.  See task 915.
    SessionSucceeded { successor_id: String },
    /// Response to [`Request::GetTaskSessionRole`].  When `is_worker`
    /// is true the session is currently bound to a non-terminal task in
    /// the role identified by `role` (typically `"worker"`); `task_id`
    /// names the task.  When `is_worker` is false (no task linkage),
    /// `task_id` and `role` are `None`.  See task 915.
    TaskSessionRole {
        is_worker: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    /// Error.
    Error { message: String },
}

/// Sentinel message used by `Response::Error { message }` when a request
/// was refused because the server is in the shutdown-drain window.
///
/// Clients recognise this exact string to distinguish "server is
/// transitioning" (reconnect + retry) from "this specific operation
/// failed" (surface the error). Kept as a plain constant rather than a
/// dedicated enum variant so that older clients that only know about
/// `Response::Error` still see a human-readable message in the UI.
pub const SHUTTING_DOWN_ERROR: &str = "__tau_server_shutting_down__";

/// Returns true if `err` is the distinctive "server is shutting down"
/// signal produced by the server during its drain window. Used by
/// clients to trigger reconnect/retry paths instead of surfacing the
/// error to the user.
pub fn is_shutting_down_error(err: &str) -> bool {
    err == SHUTTING_DOWN_ERROR || err.contains("server is shutting down")
}

/// Returns true if `err` looks like a failure surfaced by the
/// Anthropic subscription-usage poll path (`/v1/messages/usage` /
/// `/api/oauth/usage`). Used by clients as defence-in-depth: the
/// server-side handler in #940 no longer sends `Response::Error` for
/// these failures — it falls back to a cached or default
/// [`Response::SubscriptionUsage`] — but if a future code path were to
/// regress and emit such an error over the wire, clients can
/// recognize it as out-of-band and refrain from tearing down
/// streaming UI state (in-flight tool calls, agent phase, etc.).
pub fn is_subscription_usage_error(err: &str) -> bool {
    err.contains("usage API")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub cwd: Option<String>,
    pub message_count: usize,
    pub stats: SessionStats,
    /// Unix timestamp (seconds) of last activity (last message or session creation).
    pub last_activity: i64,
    /// Parent session ID (None for root sessions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Number of direct child sessions.
    #[serde(default)]
    pub child_count: usize,
    /// Budget for descendant sessions.
    #[serde(default)]
    pub child_budget: u32,
    /// Short description of what this session is working on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    /// Current agent phase: "idle", "thinking", "responding", "tool_exec", etc.
    #[serde(default = "default_state")]
    pub state: String,
    /// Context usage as percentage (0-100), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_pct: Option<f64>,
    /// Whether this session is archived.
    #[serde(default)]
    pub archived: bool,
    /// Project name this session belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// Optional successor session id.  When `Some`, this session has
    /// been retired and notifications targeted at it are forwarded to
    /// the resolved tip of the successor chain.  See task 914.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<String>,
    /// Last exit status: null (never ran), "completed", "error", "cancelled", "max_turns".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit_status: Option<String>,
    /// True when a chat turn is actively running for this session right now.
    /// False means the session is idle — `state` may reflect a stale phase
    /// from a previous turn or server restart.
    #[serde(default)]
    pub is_live: bool,
    /// Unix-ms timestamp when the current non-Idle turn began on the
    /// server. `Some(_)` while a turn is in flight, `None` when the
    /// session is idle. Used by the TUI to anchor the "Working... Xs"
    /// counter so it remains correct when attaching to an already-running
    /// session from the picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at_ms: Option<u64>,
    /// Unix-ms timestamp when the current phase began on the server.
    /// Re-stamped on every phase transition within a turn; `None` when
    /// the session is idle. Symmetric to `turn_started_at_ms` so
    /// late-subscribing clients can anchor the per-phase elapsed counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_started_at_ms: Option<u64>,
}

/// Result for a single session in WaitSessions response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session_id: String,
    /// "done", "error", "cancelled", "timeout"
    pub status: String,
    /// Last assistant message text (truncated).
    pub summary: String,
}

fn default_wait_timeout() -> u64 {
    300
}

fn default_true() -> bool {
    true
}

fn default_state() -> String {
    "idle".into()
}

fn default_recent_limit() -> usize {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub thinking: crate::types::ThinkingStyle,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// One configured alias entry: a short name pointing at a target.
///
/// Targets are model ids, optionally prefixed with `provider/`.  See
/// [`crate::model_resolve`] for resolution rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasInfo {
    /// The short name users type, e.g. `"smart"`.
    pub name: String,
    /// What the alias points at, e.g. `"claude-opus-4-6"` or
    /// `"openai/gpt-4.1-mini"`.
    pub target: String,
}

/// Task info for wire protocol (mirrors tasks_db::Task but protocol-owned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: i64,
    pub project_name: String,
    pub title: String,
    pub state: String,
    pub priority: i64,
    pub parent_id: Option<i64>,
    pub tags: Option<serde_json::Value>,
    pub affected_files: Option<serde_json::Value>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub skip_review: bool,
    pub require_approval: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default)]
    pub held: bool,
    /// Best-effort hint: true when any session recorded on this task is
    /// currently running a chat turn.  Populated server-side for the
    /// TaskList / TaskTree / TaskDetail responses; defaults to false for
    /// back-compat with older clients / serialised payloads.
    #[serde(default)]
    pub has_live_session: bool,
    /// Project the task was *filed from* — the calling session's
    /// project at `task_create` time. Distinct from
    /// [`TaskInfo::project_name`], which is where the work runs. Equal
    /// for same-project filing, different for cross-project filing
    /// (#750). `None` for tasks created before #758. See
    /// `tasks_db::Task::filed_by_project` for full semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filed_by_project: Option<String>,
    /// Session id of the caller that ran `task_create`. `None` for
    /// tasks created before #758, or when no calling session was
    /// available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filed_by_session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Task message info for wire protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessageInfo {
    pub id: i64,
    pub task_id: i64,
    pub content: String,
    pub author: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Task relation info for wire protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRelationInfo {
    pub from_task: i64,
    pub to_task: i64,
    pub relation: String,
}

/// Session recorded against a task, enriched with best-effort live state.
///
/// One `TaskSessionInfo` per `(task_id, session_id, role)` row in
/// `task_sessions`.  Enrichment fields are `Option<T>` because a session may
/// have been deleted, archived to a different store, or be otherwise
/// unreadable — we still want to surface the bare `(session_id, role)` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSessionInfo {
    /// Session ID.
    pub session_id: String,
    /// Role: "creator" | "interactive" | "planner" | "refiner" | "worker" | "reviewer".
    pub role: String,
    /// When this session was recorded on the task (unix millis).
    pub created_at: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Unix seconds of the session's last activity (any message append).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<i64>,
    /// Last known phase ("idle" | "thinking" | "responding" | "tool_exec" | ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_phase: Option<String>,
    /// Exit status if the session has finished a turn:
    /// "completed" | "error" | "cancelled" | "max_turns".  None while live or
    /// if the session has never run a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_status: Option<String>,
    /// True when a chat turn is actively running for this session right now.
    #[serde(default)]
    pub is_live: bool,
}

/// Per-task wait-reason bundle attached to a `TaskOverview` response.
///
/// The scheduler classifies every non-dispatched task into one or more
/// [`TaskWaitReason`]s; the TUI uses this both for inline `⏳ #N`
/// suffixes (dependency reasons) and the full detail-overlay list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWaitReasons {
    pub task_id: i64,
    pub reasons: Vec<TaskWaitReason>,
}

/// Why a task is waiting / not yet dispatched. Mirrors the plugin-side
/// `WaitReason` enum in `tau-agent-plugin-tasks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskWaitReason {
    /// Blocked by a dependency that hasn't completed yet.
    Dependency {
        task_id: i64,
        title: String,
        state: String,
        project_name: String,
    },
    /// Affected files overlap with an active/in-flight task.
    FileConflict {
        files: Vec<String>,
        with_task_id: i64,
    },
    /// Concurrent task budget exhausted.
    BudgetExhausted { used: usize, max: usize },
    /// The merge_target branch does not exist in the repository.
    MergeTargetNotFound { branch: String },
    /// In ready/planning state but not yet scheduled.
    NotScheduled,
}

/// Entry in the task history log (`task_history` table).

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHistoryInfo {
    /// Field that was updated: "state", "priority", "held", "affected_files",
    /// "title", ...
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_value: Option<String>,
    /// Session that performed the update, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unix millis.
    pub created_at: i64,
}

/// Cumulative session usage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStats {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub tokens: TokenStats,
    pub cost: f64,
    /// Whether credentials are OAuth (subscription).
    pub is_subscription: bool,
    /// Context window info from the model.
    pub context_window: u64,
    /// Estimated context usage from last assistant response (input tokens).
    pub context_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenStats {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// Project-wide usage / cost totals, aggregated across every session
/// (archived included) belonging to a project.
///
/// Returned by the `ProjectStats` request.  No per-model breakdown in v1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectStatsInfo {
    pub project_name: String,
    pub session_count: usize,
    pub message_count: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost_usd: f64,
    /// Unix-seconds timestamp of the most recent message in any of the
    /// project's sessions, or `None` if the project has no messages yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<i64>,
}

/// Project metadata returned by the `GetProjectInfo` request.
///
/// Currently a thin wrapper over the DB row; only `name` and `path` are
/// surfaced because callers (e.g. the worker bash fallback) only need
/// the root path. Extend as needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfoEntry {
    pub name: String,
    pub path: String,
}

/// Format a token count for display: 1234 → "1.2K", 1234567 → "1.2M".
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format session stats as a compact one-line summary like pi's footer:
/// `↑12K ↓81K R18M W353K $13.434 (sub) 18.4%/200K`
#[allow(clippy::cast_precision_loss)]
pub fn format_stats(stats: &SessionStats) -> String {
    let mut parts = Vec::new();

    if stats.tokens.input > 0 {
        parts.push(format!("↑{}", format_tokens(stats.tokens.input)));
    }
    if stats.tokens.output > 0 {
        parts.push(format!("↓{}", format_tokens(stats.tokens.output)));
    }
    if stats.tokens.cache_read > 0 {
        parts.push(format!("R{}", format_tokens(stats.tokens.cache_read)));
    }
    if stats.tokens.cache_write > 0 {
        parts.push(format!("W{}", format_tokens(stats.tokens.cache_write)));
    }

    let cost_str = if stats.is_subscription {
        format!("${:.3} (sub)", stats.cost)
    } else if stats.cost > 0.0 {
        format!("${:.3}", stats.cost)
    } else {
        String::new()
    };
    if !cost_str.is_empty() {
        parts.push(cost_str);
    }

    if stats.context_window > 0 {
        let ctx = match stats.context_tokens {
            Some(t) => {
                let pct = (t as f64 / stats.context_window as f64) * 100.0;
                format!("{:.1}%/{}", pct, format_tokens(stats.context_window))
            }
            None => format!("?/{}", format_tokens(stats.context_window)),
        };
        parts.push(ctx);
    }

    parts.join(" ")
}

/// Format a `resets_at` ISO-8601 timestamp as a compact time-until-reset string.
/// Returns "?" if the timestamp can't be parsed or is in the past.
fn format_resets_at(resets_at: &str) -> String {
    // Parse ISO-8601 timestamps like "2026-04-03T18:30:00Z" or with fractional seconds.
    // We do minimal parsing to avoid pulling in chrono.
    let trimmed = resets_at.trim().trim_end_matches('Z');
    let (date_part, time_part) = match trimmed.split_once('T') {
        Some(pair) => pair,
        None => return "?".into(),
    };
    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let month: i64 = date_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let day: i64 = date_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    // Strip fractional seconds and timezone offset beyond 'Z'
    let time_clean = time_part
        .split('+')
        .next()
        .unwrap_or(time_part)
        .split('.')
        .next()
        .unwrap_or(time_part);
    let mut time_iter = time_clean.split(':');
    let hour: i64 = time_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minute: i64 = time_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let second: i64 = time_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    // Convert to Unix timestamp (approximate — ignores leap seconds, good enough for display).
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }
    let reset_epoch =
        days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let delta = reset_epoch - now;
    if delta <= 0 {
        return "?".into();
    }
    format_duration_compact(delta)
}

/// Format seconds as compact duration: "16h", "2d", "45m".
fn format_duration_compact(secs: i64) -> String {
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// Format utilization (already 0–100 from the API) as a compact percentage string.
pub fn format_utilization(utilization: Option<f64>) -> String {
    match utilization {
        Some(u) => format!("{:.0}%", u),
        None => "?".into(),
    }
}

/// Format a single usage bucket as `"LABEL PCT RESET"`.
fn format_usage_bucket(label: &str, bucket: &UsageBucket) -> Option<String> {
    let pct = format_utilization(bucket.utilization);
    if pct == "?" {
        return None;
    }
    let reset = bucket
        .resets_at
        .as_deref()
        .map(format_resets_at)
        .unwrap_or_else(|| "?".into());
    Some(format!("{} {} {}", label, pct, reset))
}

/// Format subscription usage as a compact footer string.
///
/// Example: `(5h 50% 16h | 7d 12% 2d | sonnet 6% 1d)`
///
/// Returns `None` if there's no usage data to display.
pub fn format_subscription_usage(usage: &SubscriptionUsage) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref b) = usage.five_hour
        && let Some(s) = format_usage_bucket("5h", b)
    {
        parts.push(s);
    }
    if let Some(ref b) = usage.seven_day
        && let Some(s) = format_usage_bucket("7d", b)
    {
        parts.push(s);
    }
    if let Some(ref b) = usage.seven_day_sonnet
        && let Some(s) = format_usage_bucket("sonnet", b)
    {
        parts.push(s);
    }
    if let Some(ref b) = usage.seven_day_opus
        && let Some(s) = format_usage_bucket("opus", b)
    {
        parts.push(s);
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("({})", parts.join(" | ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_units() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(999_999), "1000.0K");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(18_500_000), "18.5M");
    }

    #[test]
    fn format_stats_empty() {
        let stats = SessionStats::default();
        assert_eq!(format_stats(&stats), "");
    }

    #[test]
    fn format_stats_basic() {
        let stats = SessionStats {
            tokens: TokenStats {
                input: 12_000,
                output: 81_000,
                cache_read: 18_000_000,
                cache_write: 353_000,
            },
            cost: 13.434,
            is_subscription: true,
            context_window: 200_000,
            context_tokens: Some(36_800),
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("↑12.0K"), "got: {s}");
        assert!(s.contains("↓81.0K"), "got: {s}");
        assert!(s.contains("R18.0M"), "got: {s}");
        assert!(s.contains("W353.0K"), "got: {s}");
        assert!(s.contains("$13.434 (sub)"), "got: {s}");
        assert!(s.contains("18.4%/200.0K"), "got: {s}");
    }

    #[test]
    fn format_stats_unknown_context() {
        let stats = SessionStats {
            context_window: 200_000,
            context_tokens: None,
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("?/200.0K"), "got: {s}");
    }

    #[test]
    fn format_stats_no_subscription() {
        let stats = SessionStats {
            tokens: TokenStats {
                input: 500,
                output: 200,
                ..Default::default()
            },
            cost: 0.005,
            is_subscription: false,
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("$0.005"), "got: {s}");
        assert!(!s.contains("(sub)"), "got: {s}");
    }

    #[test]
    fn format_subscription_usage_basic() {
        use crate::subscription_usage::{SubscriptionUsage, UsageBucket};
        let usage = SubscriptionUsage {
            five_hour: Some(UsageBucket {
                utilization: Some(50.0),
                resets_at: Some("2099-01-01T16:00:00Z".into()),
            }),
            seven_day: Some(UsageBucket {
                utilization: Some(12.0),
                resets_at: Some("2099-01-03T00:00:00Z".into()),
            }),
            seven_day_sonnet: Some(UsageBucket {
                utilization: Some(6.0),
                resets_at: Some("2099-01-02T00:00:00Z".into()),
            }),
            seven_day_opus: None,
            extra_usage: None,
        };
        let s = format_subscription_usage(&usage).unwrap();
        assert!(s.starts_with('('), "got: {s}");
        assert!(s.ends_with(')'), "got: {s}");
        assert!(s.contains("5h 50%"), "got: {s}");
        assert!(s.contains("7d 12%"), "got: {s}");
        assert!(s.contains("sonnet 6%"), "got: {s}");
        assert!(s.contains(" | "), "got: {s}");
    }

    #[test]
    fn format_subscription_usage_empty() {
        use crate::subscription_usage::SubscriptionUsage;
        let usage = SubscriptionUsage::default();
        assert!(format_subscription_usage(&usage).is_none());
    }

    #[test]
    fn format_subscription_usage_no_utilization() {
        use crate::subscription_usage::{SubscriptionUsage, UsageBucket};
        let usage = SubscriptionUsage {
            five_hour: Some(UsageBucket {
                utilization: None,
                resets_at: Some("2099-01-01T16:00:00Z".into()),
            }),
            ..Default::default()
        };
        // Bucket with no utilization is skipped
        assert!(format_subscription_usage(&usage).is_none());
    }

    #[test]
    fn format_duration_compact_units() {
        assert_eq!(format_duration_compact(30), "30s");
        assert_eq!(format_duration_compact(90), "1m");
        assert_eq!(format_duration_compact(3600), "1h");
        assert_eq!(format_duration_compact(7200), "2h");
        assert_eq!(format_duration_compact(86400), "1d");
        assert_eq!(format_duration_compact(172800), "2d");
    }

    /// Verify that all new task-related protocol variants round-trip through serde.
    #[test]
    fn task_protocol_serde_roundtrip() {
        let task = TaskInfo {
            id: 42,
            project_name: "test-project".into(),
            title: "test task".into(),
            state: "active".into(),
            priority: 5,
            parent_id: Some(1),
            tags: Some(serde_json::json!(["foo", "bar"])),
            affected_files: None,
            branch: Some("task-42".into()),
            worktree_path: None,
            session_id: Some("s123".into()),
            skip_review: false,
            require_approval: false,
            sandbox_profile: None,
            held: false,
            has_live_session: false,
            filed_by_project: None,
            filed_by_session_id: None,
            created_at: 1000,
            updated_at: 2000,
        };
        let msg = TaskMessageInfo {
            id: 1,
            task_id: 42,
            content: "hello".into(),
            author: Some("test".into()),
            created_at: 1000,
            updated_at: 2000,
        };
        let rel = TaskRelationInfo {
            from_task: 42,
            to_task: 43,
            relation: "depends_on".into(),
        };

        // Request variants
        let requests: Vec<Request> = vec![
            Request::SetTagline {
                session_id: "s1".into(),
                tagline: "hi".into(),
            },
            Request::TaskList {
                project: "/tmp".into(),
                state: Some("active".into()),
                parent_id: None,
            },
            Request::TaskGet { id: 42 },
            Request::TaskCreate {
                project: "/tmp".into(),
                title: "new".into(),
                parent_id: None,
                priority: Some(3),
                tags: vec!["a".into()],
                sandbox_profile: None,
            },
            Request::TaskUpdate {
                id: 42,
                state: Some("approved".into()),
                title: None,
                priority: None,
                tags: None,
                affected_files: None,
                skip_review: None,
                require_approval: None,
                sandbox_profile: None,
            },
            Request::TaskSearch {
                project: "/tmp".into(),
                query: "test".into(),
                state: None,
            },
            Request::TaskAssign {
                id: 42,
                session_id: "s1".into(),
            },
            Request::TaskStatus {
                project: "/tmp".into(),
            },
            Request::TaskOverview {
                project: "/tmp".into(),
                recent_limit: 5,
            },
            Request::TaskMergeQueue {
                project: "/tmp".into(),
            },
            Request::ProjectStats {
                project_name: "tau".into(),
            },
            Request::GetProjectInfo {
                project_name: "tau".into(),
            },
            Request::SetSessionSuccessor {
                session_id: "s1".into(),
                successor_id: Some("s2".into()),
                caller_session_id: None,
            },
            Request::SetSessionSuccessor {
                session_id: "s1".into(),
                successor_id: None,
                caller_session_id: Some("caller".into()),
            },
            Request::ResolveSuccessor {
                session_id: "s1".into(),
            },
            Request::SucceedSession {
                session_id: "s1".into(),
                tagline: Some("continued".into()),
                caller_session_id: Some("caller".into()),
            },
            Request::SucceedSession {
                session_id: "s1".into(),
                tagline: None,
                caller_session_id: None,
            },
            Request::GetTaskSessionRole {
                session_id: "s1".into(),
            },
        ];
        for req in &requests {
            let json = serde_json::to_string(req).expect("serialize request");
            let _: Request = serde_json::from_str(&json).expect("deserialize request");
        }

        // Response variants
        let responses: Vec<Response> = vec![
            Response::TaskList {
                tasks: vec![task.clone()],
            },
            Response::TaskDetail {
                task: task.clone(),
                messages: vec![msg],
                relations: vec![rel],
                subtasks: vec![task.clone()],
                sessions: Vec::new(),
                history: Vec::new(),
            },
            Response::TaskUpdated { task: task.clone() },
            Response::TaskStatus {
                text: "status text".into(),
            },
            Response::TaskOverview {
                active: vec![task.clone()],
                queued_ready: Vec::new(),
                queued_planning: Vec::new(),
                blocked: Vec::new(),
                held: Vec::new(),
                recently_merged: Vec::new(),
                recently_closed: Vec::new(),
                inflight_count: 1,
                max_concurrent: 8,
                wait_reasons: vec![TaskWaitReasons {
                    task_id: 99,
                    reasons: vec![
                        TaskWaitReason::Dependency {
                            task_id: 42,
                            title: "dep".into(),
                            state: "active".into(),
                            project_name: "tau".into(),
                        },
                        TaskWaitReason::BudgetExhausted { used: 8, max: 8 },
                    ],
                }],
            },
            Response::TaskTree {
                tasks: vec![(0, task.clone())],
            },
            Response::TaskMergeQueue { tasks: vec![task] },
            Response::ProjectStats {
                stats: ProjectStatsInfo {
                    project_name: "tau".into(),
                    session_count: 42,
                    message_count: 8124,
                    tokens_input: 12_340_156,
                    tokens_output: 418_902,
                    tokens_cache_read: 34_521_088,
                    tokens_cache_write: 2_108_445,
                    cost_usd: 28.47,
                    last_activity: Some(1_700_000_000),
                },
            },
            Response::ProjectInfo {
                project: Some(ProjectInfoEntry {
                    name: "tau".into(),
                    path: "/home/u/src/tau".into(),
                }),
            },
            Response::ProjectInfo { project: None },
            Response::ResolvedSuccessor {
                session_id: "s1".into(),
            },
            Response::SessionSucceeded {
                successor_id: "s2".into(),
            },
            Response::TaskSessionRole {
                is_worker: true,
                task_id: Some(42),
                role: Some("worker".into()),
            },
            Response::TaskSessionRole {
                is_worker: false,
                task_id: None,
                role: None,
            },
        ];
        for resp in &responses {
            let json = serde_json::to_string(resp).expect("serialize response");
            let _: Response = serde_json::from_str(&json).expect("deserialize response");
        }
    }

    #[test]
    fn shutting_down_error_round_trips_through_response() {
        let err = Response::Error {
            message: SHUTTING_DOWN_ERROR.into(),
        };
        let wire = serde_json::to_string(&err).expect("serialize");
        let parsed: Response = serde_json::from_str(&wire).expect("deserialize");
        match parsed {
            Response::Error { message } => {
                assert!(is_shutting_down_error(&message));
            }
            other => panic!("unexpected variant: {:?}", other),
        }

        assert!(is_shutting_down_error(SHUTTING_DOWN_ERROR));
        assert!(is_shutting_down_error("server is shutting down"));
        assert!(!is_shutting_down_error("some other error"));
    }

    #[test]
    fn chat_serialises_without_attachments_when_empty() {
        let req = Request::Chat {
            session_id: "s1".into(),
            text: "hi".into(),
            attachments: Vec::new(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("attachments"),
            "empty attachments should be omitted from JSON, got: {json}"
        );
        let parsed: Request = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Request::Chat {
                session_id,
                text,
                attachments,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(text, "hi");
                assert!(attachments.is_empty());
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn chat_with_image_roundtrips() {
        let req = Request::Chat {
            session_id: "s1".into(),
            text: "describe".into(),
            attachments: vec![ChatAttachment::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }],
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("\"attachments\""), "got: {json}");
        assert!(json.contains("\"type\":\"image\""), "got: {json}");
        assert!(json.contains("\"mime_type\":\"image/png\""), "got: {json}");
        let parsed: Request = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Request::Chat {
                session_id,
                text,
                attachments,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(text, "describe");
                assert_eq!(attachments.len(), 1);
                match &attachments[0] {
                    ChatAttachment::Image { data, mime_type } => {
                        assert_eq!(data, "AAAA");
                        assert_eq!(mime_type, "image/png");
                    }
                }
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn legacy_chat_payload_deserialises() {
        // Old client payloads omit the `attachments` field entirely.
        let json = r#"{"type":"chat","session_id":"s","text":"hi"}"#;
        let parsed: Request = serde_json::from_str(json).expect("deserialize legacy");
        match parsed {
            Request::Chat {
                session_id,
                text,
                attachments,
            } => {
                assert_eq!(session_id, "s");
                assert_eq!(text, "hi");
                assert!(attachments.is_empty());
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }
}
