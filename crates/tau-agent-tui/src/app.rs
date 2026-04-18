//! Application state for the TUI.

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use tau_agent::auth::SubscriptionUsage;
use tau_agent::protocol::{
    Response, SessionInfo, TaskHistoryInfo, TaskInfo, TaskMessageInfo, TaskRelationInfo,
    TaskSessionInfo,
};
use tau_agent::types::{
    AgentPhase, AssistantContent, Message, StopReason, StreamEvent, ToolResultMessage, UserContent,
};

use crate::events::Event;
use crate::message::MessageItem;
use crate::render::RendererRegistry;
use crate::theme::Theme;

/// Cumulative usage tracking (mirrored from tau-agent-cli).
#[derive(Default)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub context_window: u64,
    pub context_tokens: Option<u64>,
    pub is_subscription: bool,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &tau_agent::Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
        self.context_tokens = Some(usage.input + usage.cache_read + usage.cache_write);
    }
}

/// What the app is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Waiting for user input.
    Input,
    /// Streaming a response from the LLM.
    Streaming,
    /// Session picker overlay is open.
    SessionPicker,
    /// Task picker overlay is open.
    TaskPicker,
}

/// Which task-picker action is waiting for y/n confirmation.
#[derive(Debug, Clone, Copy)]
pub enum TaskPickerConfirmAction {
    Approve,
    Ready,
    Dispatch,
    Merge,
}

/// Detailed view of a single task in the task picker.
pub struct TaskPickerDetail {
    pub task: TaskInfo,
    pub messages: Vec<TaskMessageInfo>,
    pub relations: Vec<TaskRelationInfo>,
    pub subtasks: Vec<TaskInfo>,
    pub sessions: Vec<TaskSessionInfo>,
    pub history: Vec<TaskHistoryInfo>,
    pub scroll: usize,
}

/// Application state.
pub struct App {
    /// Theme for rendering.
    pub theme: Theme,
    /// Tool renderer registry.
    pub renderers: RendererRegistry,
    /// Session ID we're chatting in.
    pub session_id: String,
    /// Model name for display.
    pub model: String,
    /// Provider name for display.
    pub provider: String,
    /// Chat message history.
    pub messages: Vec<MessageItem>,
    /// Current mode.
    pub mode: AppMode,
    /// Current agent phase — updated explicitly by Phase events and
    /// implicitly by stream events (see `update_phase_from_event`).
    pub phase: AgentPhase,
    /// Scroll position. None = follow bottom (auto-scroll). Some(pos) = pinned at line pos from top.
    pub scroll_pos: std::cell::Cell<Option<usize>>,
    /// Max scroll value from last render (set during draw via Cell).
    pub max_scroll: std::cell::Cell<usize>,
    /// Usage totals.
    pub totals: UsageTotals,
    /// Should the app quit?
    pub should_quit: bool,
    /// Text area for input.
    pub textarea: TextArea<'static>,
    /// Spinner frame counter.
    pub spinner_frame: usize,
    /// Tick sub-counter for slowing spinner during rate limits.
    pub tick_counter: usize,
    /// Last escape press time for double-escape detection.
    pub last_escape: std::time::Instant,
    /// Command history index (None = composing new, Some(i) = browsing history).
    pub history_index: Option<usize>,
    /// Saved text when entering history browse (restored on down past end).
    pub history_saved_text: String,
    /// Whether to fetch subscription usage after session info.
    pub pending_subscription_usage: bool,
    /// Cached subscription usage data for footer display.
    pub subscription_usage: Option<SubscriptionUsage>,
    /// Last time subscription usage was fetched.
    pub last_usage_fetch: std::time::Instant,
    /// Server stream ended.
    pub server_done: bool,
    /// Navigation stack for session switching (previous session IDs).
    pub nav_stack: Vec<NavEntry>,
    /// Parent session ID (if this is a child session).
    pub parent_id: Option<String>,
    /// Number of direct child sessions.
    pub child_count: usize,
    /// Working directory of the current session (used as project key for task queries).
    pub session_cwd: Option<String>,
    /// Project name of the current session.
    pub session_project_name: Option<String>,
    /// Session list for the picker overlay.
    pub picker_sessions: Vec<SessionInfo>,
    /// Cursor position in the picker.
    pub picker_cursor: usize,
    /// Pending deletion confirmation: Some(index) if waiting for y/n.
    pub picker_confirm_delete: Option<usize>,
    /// Pending archive confirmation: Some(index) if waiting for y/n.
    pub picker_confirm_archive: Option<usize>,
    /// Mode to restore when the session picker is closed.
    pub picker_previous_mode: AppMode,
    /// Search filter text for the session picker.
    pub picker_filter: String,
    /// Whether the picker is in filter-input mode (`/` was pressed).
    pub picker_filter_mode: bool,
    /// Tagline edit mode: Some((cursor_in_picker, current_text)) when editing.
    /// The third element is the text cursor position (byte offset).
    pub picker_edit_tagline: Option<(usize, String, usize)>,
    /// Set of session IDs whose subtrees are folded (children hidden).
    pub picker_folded: std::collections::HashSet<String>,
    /// Project filter for session picker. When Some, only show sessions for this project.
    pub picker_project_filter: Option<String>,
    /// Whether the picker is showing all sessions (true) or project-filtered (false).
    pub picker_show_all_projects: bool,
    /// Task list for the picker overlay (tree-ordered with depth).
    pub picker_tasks: Vec<(usize, TaskInfo)>,
    /// Cursor position in the task picker (into filtered indices).
    pub task_picker_cursor: usize,
    /// Mode to restore when the task picker is closed.
    pub task_picker_previous_mode: AppMode,
    /// Search filter text for the task picker.
    pub task_picker_filter: String,
    /// Whether the task picker is in filter-input mode (`/` was pressed).
    pub task_picker_filter_mode: bool,
    /// Whether the filter bar is in "create" mode (`c` was pressed; Enter creates task).
    pub task_picker_create_mode: bool,
    /// Pending confirmation: (cursor_pos, label, which_action) for y/n prompt.
    pub task_picker_confirm: Option<(usize, String, TaskPickerConfirmAction)>,
    /// Task detail view data (when Enter is pressed on a task).
    pub task_picker_detail: Option<Box<TaskPickerDetail>>,
    /// Pending `/task switch <id>` — set when the user ran the command and the
    /// TaskDetail response hasn't come back yet.  When the response arrives we
    /// resolve the primary session and emit `Action::SwitchSession`.
    pub pending_task_switch: Option<i64>,
    /// Task ID and title assigned to the current session (if any), for footer display.
    pub current_task_id: Option<(i64, String)>,
    /// Whether a text block in the current turn has already been
    /// finalized by a `StreamEvent::TextEnd`.  Used by `StreamEvent::Done`
    /// to suppress its fallback append branch when `TextEnd` has already
    /// converted the in-flight placeholder to its final `Assistant` form
    /// — without this flag the fallback would duplicate every text-only
    /// turn (regression from task #421).  Reset at the end of each turn
    /// and by `Start`/`Error`.
    pub turn_text_finalized: bool,
    /// Most recent steer message text (shown above the input box).
    /// Cleared when the next assistant turn starts (`StreamEvent::Start`).
    pub pending_steer: Option<String>,

    /// Whether tool outputs are expanded (showing full output) or collapsed (showing summary).
    pub all_tools_expanded: bool,

    /// Local input history for arrow-key scrollback.
    /// Includes both regular chat messages and slash commands.
    pub input_history: Vec<String>,
}

/// Saved state when navigating to a child session.
pub struct NavEntry {
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub messages: Vec<MessageItem>,
    pub totals: UsageTotals,
    pub parent_id: Option<String>,
    pub child_count: usize,
    pub subscription_usage: Option<SubscriptionUsage>,
    pub last_usage_fetch: std::time::Instant,
    /// Working directory of the session at the time of navigation.
    pub session_cwd: Option<String>,
    /// Project name of the session at the time of navigation.
    pub session_project_name: Option<String>,
}

impl App {
    pub fn new(session_id: String, model: String, provider: String, theme: Theme) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(ratatui::style::Style::default());

        Self {
            theme,
            renderers: RendererRegistry::new(),
            session_id,
            model,
            provider,
            messages: Vec::new(),
            mode: AppMode::Input,
            phase: AgentPhase::default(),
            scroll_pos: std::cell::Cell::new(None),
            max_scroll: std::cell::Cell::new(0),

            totals: UsageTotals::default(),
            should_quit: false,
            textarea,
            spinner_frame: 0,
            tick_counter: 0,
            last_escape: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .expect("10s subtraction should not underflow Instant"),
            history_index: None,
            history_saved_text: String::new(),
            pending_subscription_usage: false,
            subscription_usage: None,
            last_usage_fetch: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .expect("3600s subtraction should not underflow Instant"),
            server_done: false,
            nav_stack: Vec::new(),
            parent_id: None,
            child_count: 0,
            session_cwd: None,
            session_project_name: None,
            picker_sessions: Vec::new(),
            picker_cursor: 0,
            picker_confirm_delete: None,
            picker_confirm_archive: None,
            picker_previous_mode: AppMode::Input,
            picker_filter: String::new(),
            picker_filter_mode: false,
            picker_edit_tagline: None,
            picker_folded: std::collections::HashSet::new(),
            picker_project_filter: None,
            picker_show_all_projects: false,
            picker_tasks: Vec::new(),
            task_picker_cursor: 0,
            task_picker_previous_mode: AppMode::Input,
            task_picker_filter: String::new(),
            task_picker_filter_mode: false,
            task_picker_create_mode: false,
            task_picker_confirm: None,
            task_picker_detail: None,
            pending_task_switch: None,
            current_task_id: None,
            turn_text_finalized: false,
            pending_steer: None,
            all_tools_expanded: false,
            input_history: Vec::new(),
        }
    }

    /// Populate message history from stored messages (for session resume).
    pub fn restore_messages(&mut self, messages: &[Message]) {
        // Build a map of tool_call_id -> arguments from assistant messages
        // so we can recover args when displaying ToolResult entries.
        let mut tool_call_args: std::collections::HashMap<&str, &serde_json::Value> =
            std::collections::HashMap::new();
        for msg in messages {
            if let Message::Assistant(a) = msg {
                for content in &a.content {
                    if let AssistantContent::ToolCall(tc) = content {
                        tool_call_args.insert(&tc.id, &tc.arguments);
                    }
                }
            }
        }

        for msg in messages {
            match msg {
                Message::User(user_msg) => {
                    let text = user_msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            UserContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        self.push_input_history(&text);
                        self.messages.push(MessageItem::User { text });
                    }
                }
                Message::Assistant(assistant_msg) => {
                    let text = assistant_msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            AssistantContent::Text(t) if !t.text.is_empty() => {
                                Some(t.text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        self.messages.push(MessageItem::Assistant { text });
                    }
                    self.totals.add(&assistant_msg.usage);
                }
                Message::ToolResult(ToolResultMessage {
                    tool_call_id,
                    tool_name,
                    is_error,
                    content,
                    duration_ms,
                    summary,
                    ..
                }) => {
                    let output = content
                        .iter()
                        .filter_map(|c| match c {
                            tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let args = tool_call_args
                        .get(tool_call_id.as_str())
                        .cloned()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    self.messages.push(MessageItem::ToolComplete {
                        name: tool_name.clone(),
                        args,
                        output,
                        is_error: *is_error,
                        duration: duration_ms.map(std::time::Duration::from_millis),
                        summary: summary.clone(),
                        expanded: self.all_tools_expanded,
                    });
                }
                Message::CompactionSummary(_) => {
                    // Skip compaction summaries in the UI
                }
                Message::Info(info_msg) => {
                    if !info_msg.text.is_empty() {
                        self.messages.push(MessageItem::Status {
                            text: format!("ℹ {}", info_msg.text),
                        });
                    }
                }
            }
        }
    }

    /// Push a raw input line into the local input history.
    fn push_input_history(&mut self, text: &str) {
        // Deduplicate: skip if identical to the most recent entry.
        if self.input_history.last().map(|s| s.as_str()) != Some(text) {
            self.input_history.push(text.to_string());
        }
    }

    /// Set textarea content from a string.
    fn set_textarea_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(text);
    }

    /// Scroll up by N lines (pins viewport if not already pinned).
    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.max_scroll.get();
        let current_top = match self.scroll_pos.get() {
            Some(pos) => pos,
            None => max,
        };
        self.scroll_pos.set(Some(current_top.saturating_sub(lines)));
    }

    /// Scroll down by N lines. Use scroll_to_bottom() / End to unpin.
    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(pos) = self.scroll_pos.get() {
            self.scroll_pos.set(Some(pos.saturating_add(lines)));
        }
    }

    /// Jump to bottom (unpin).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_pos.set(None);
    }

    /// Jump to top.
    pub fn scroll_to_top(&mut self) {
        self.scroll_pos.set(Some(0));
    }

    /// Whether the user is scrolled up (not following bottom).
    pub fn is_scrolled(&self) -> bool {
        self.scroll_pos.get().is_some()
    }

    /// Handle an event, returning an optional request to send to the server.
    pub fn handle_event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Terminal(ct_event) => self.handle_terminal_event(ct_event),
            Event::Server(response) => {
                let action = self.handle_server_response(*response);
                if action.is_some() {
                    return action;
                }
                // Fetch subscription usage if requested
                if self.pending_subscription_usage {
                    self.pending_subscription_usage = false;
                    return Some(Action::GetSubscriptionUsage);
                }
                None
            }
            Event::ServerDone => {
                self.server_done = true;
                None
            }
            Event::Tick => {
                if self.mode == AppMode::Streaming {
                    // Slow spinner during rate limit (advance ~2x/sec instead of ~15x/sec)
                    if self.phase == AgentPhase::RateLimited {
                        self.tick_counter += 1;
                        if self.tick_counter >= 8 {
                            self.tick_counter = 0;
                            self.spinner_frame = self.spinner_frame.wrapping_add(1);
                        }
                    } else {
                        self.spinner_frame = self.spinner_frame.wrapping_add(1);
                    }
                }
                None
            }
        }
    }

    fn handle_terminal_event(&mut self, event: CtEvent) -> Option<Action> {
        // Handle bracketed paste: insert full text into textarea
        if let CtEvent::Paste(text) = &event {
            self.textarea.insert_str(text);
            return None;
        }

        // Only handle key press events
        let CtEvent::Key(key) = &event else {
            return None;
        };
        // Accept Press and Repeat (for key-hold), ignore Release
        if key.kind == KeyEventKind::Release {
            return None;
        }

        match self.mode {
            AppMode::Input => self.handle_input_key(key),
            AppMode::Streaming => self.handle_streaming_key(key),
            AppMode::SessionPicker => self.handle_picker_key(key),
            AppMode::TaskPicker => self.handle_task_picker_key(key),
        }
    }

    fn handle_input_key(&mut self, key: &KeyEvent) -> Option<Action> {
        match (key.code, key.modifiers) {
            // Ctrl+D on empty input: quit
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if self.textarea.lines().iter().all(|l: &String| l.is_empty()) {
                    self.should_quit = true;
                }
                None
            }
            // Ctrl+C: quit
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            // Enter: send message (unless shift/alt held for newline)
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let text: String = self.textarea.lines().join("\n");
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.textarea.select_all();
                self.textarea.cut();

                // Handle slash commands
                if text.starts_with('/') {
                    self.push_input_history(&text);
                    self.history_index = None;
                    return self.handle_slash_command(&text);
                }

                // Don't add user message locally — it arrives via Subscribe broadcast
                self.push_input_history(&text);
                self.scroll_to_bottom();
                self.mode = AppMode::Streaming;
                self.history_index = None;
                Some(Action::SendChat(text))
            }
            // Alt+Enter while idle: send immediately (same as plain Enter)
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                let text: String = self.textarea.lines().join("\n");
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.textarea.select_all();
                self.textarea.cut();

                // Handle slash commands
                if text.starts_with('/') {
                    self.push_input_history(&text);
                    self.history_index = None;
                    return self.handle_slash_command(&text);
                }

                // Session is idle — send immediately, no need to queue
                self.push_input_history(&text);
                self.scroll_to_bottom();
                self.mode = AppMode::Streaming;
                self.history_index = None;
                Some(Action::SendChat(text))
            }
            // Shift+Enter: insert newline
            (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => {
                self.textarea.insert_newline();
                None
            }
            // Catch Enter with any other modifier (shouldn't happen, but be safe)
            (KeyCode::Enter, _) => {
                // Treat as newline if any modifier is held
                self.textarea.insert_newline();
                None
            }
            // Page up/down for scrolling
            (KeyCode::PageUp, _) => {
                self.scroll_up(10);
                None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_down(10);
                None
            }
            // Up arrow: browse history when on first line
            (KeyCode::Up, KeyModifiers::NONE) => {
                let (row, _) = self.textarea.cursor();
                if row == 0 {
                    if self.input_history.is_empty() {
                        return None;
                    }
                    let new_idx = match self.history_index {
                        None => {
                            // Save current text before browsing
                            self.history_saved_text = self.textarea.lines().join("\n");
                            self.input_history.len() - 1
                        }
                        Some(i) if i > 0 => i - 1,
                        Some(_) => return None, // already at oldest
                    };
                    self.history_index = Some(new_idx);
                    let entry = self.input_history[new_idx].clone();
                    self.set_textarea_text(&entry);
                    return None;
                }
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
            // Down arrow: browse history forward or restore saved text
            (KeyCode::Down, KeyModifiers::NONE) => {
                if let Some(idx) = self.history_index {
                    if idx + 1 < self.input_history.len() {
                        self.history_index = Some(idx + 1);
                        let entry = self.input_history[idx + 1].clone();
                        self.set_textarea_text(&entry);
                    } else {
                        // Past end: restore saved text
                        self.history_index = None;
                        let saved = self.history_saved_text.clone();
                        self.set_textarea_text(&saved);
                    }
                    return None;
                }
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
            // Ctrl+U for scroll up
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.scroll_up(5);
                None
            }
            // Home: scroll to top when scrolled, else pass to textarea
            (KeyCode::Home, KeyModifiers::NONE) if self.is_scrolled() => {
                self.scroll_to_top();
                None
            }
            // End: scroll to bottom when scrolled, else pass to textarea
            (KeyCode::End, KeyModifiers::NONE) if self.is_scrolled() => {
                self.scroll_to_bottom();
                None
            }
            // F3: toggle expand/collapse all tool outputs
            (KeyCode::F(3), _) => {
                self.all_tools_expanded = !self.all_tools_expanded;
                for item in &mut self.messages {
                    if let MessageItem::ToolComplete { expanded, .. } = item {
                        *expanded = self.all_tools_expanded;
                    }
                }
                None
            }
            // TAB: open session picker
            (KeyCode::Tab, _) => Some(Action::OpenSessionPicker),
            // F2: open task picker
            (KeyCode::F(2), _) => Some(Action::OpenTaskPicker),
            // Everything else goes to textarea
            _ => {
                // Reset history browsing on any other key
                self.history_index = None;
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
        }
    }

    /// Return indices into `picker_sessions` that match the current filter.
    /// If the filter is empty, all non-hidden indices are returned. Sessions
    /// whose ancestor is folded are also excluded.
    pub fn picker_filtered_indices(&self) -> Vec<usize> {
        let hidden = self.picker_hidden_by_fold();
        let needle = self.picker_filter.to_lowercase();
        let filter_empty = self.picker_filter.is_empty();

        // Project filtering: when a project filter is set and we're not in
        // "show all" mode, restrict to sessions matching the project.
        let project_filter_active =
            self.picker_project_filter.is_some() && !self.picker_show_all_projects;

        self.picker_sessions
            .iter()
            .enumerate()
            .filter(|(idx, s)| {
                if hidden.contains(idx) {
                    return false;
                }
                // Apply project filter
                if project_filter_active {
                    if let Some(ref pf) = self.picker_project_filter {
                        if s.project_name.as_deref() != Some(pf.as_str()) {
                            return false;
                        }
                    }
                }
                if filter_empty {
                    return true;
                }
                s.id.to_lowercase().contains(&needle)
                    || s.tagline
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&needle)
                    || s.model.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Compute the set of session indices that are hidden because some
    /// ancestor session is in `picker_folded`. The folded session itself
    /// is NOT hidden — only its descendants.
    fn picker_hidden_by_fold(&self) -> std::collections::HashSet<usize> {
        use std::collections::{HashMap, HashSet};
        if self.picker_folded.is_empty() {
            return HashSet::new();
        }
        // Map session id -> index for ancestor walk.
        let id_to_idx: HashMap<&str, usize> = self
            .picker_sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();

        let mut hidden = HashSet::new();
        for (idx, s) in self.picker_sessions.iter().enumerate() {
            // Walk up parents; if any ancestor is folded, mark this idx hidden.
            let mut parent = s.parent_id.as_deref();
            while let Some(pid) = parent {
                if self.picker_folded.contains(pid) {
                    hidden.insert(idx);
                    break;
                }
                match id_to_idx.get(pid) {
                    Some(&pidx) => {
                        parent = self.picker_sessions[pidx].parent_id.as_deref();
                    }
                    None => break,
                }
            }
        }
        hidden
    }

    /// Reset transient picker state (confirm dialogs, edit, filter).
    /// Used when closing the picker.
    ///
    /// Note: `picker_folded` is intentionally NOT cleared here -- fold state
    /// persists across picker open/close within the same TUI process and is
    /// only reset on TUI restart (which clears the in-memory HashSet).
    fn picker_reset_transient(&mut self) {
        self.picker_confirm_delete = None;
        self.picker_confirm_archive = None;
        self.picker_edit_tagline = None;
        self.picker_filter.clear();
        self.picker_filter_mode = false;
    }

    /// Clamp picker_cursor to remain valid within filtered results.
    fn picker_clamp_cursor(&mut self) {
        let filtered = self.picker_filtered_indices();
        if filtered.is_empty() {
            self.picker_cursor = 0;
        } else if self.picker_cursor >= filtered.len() {
            self.picker_cursor = filtered.len() - 1;
        }
    }

    /// Resolve the picker cursor to a session index in `picker_sessions`.
    /// Returns `None` if no matching sessions or cursor is out of range.
    pub fn picker_selected_session_idx(&self) -> Option<usize> {
        let filtered = self.picker_filtered_indices();
        filtered.get(self.picker_cursor).copied()
    }

    fn handle_picker_key(&mut self, key: &KeyEvent) -> Option<Action> {
        // If in tagline edit mode, handle keys for text editing
        if let Some((_, ref mut text, ref mut cursor_pos)) = self.picker_edit_tagline {
            match key.code {
                KeyCode::Esc => {
                    self.picker_edit_tagline = None;
                    return None;
                }
                KeyCode::Enter => {
                    let new_tagline = text.clone();
                    self.picker_edit_tagline = None;
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get_mut(idx)
                    {
                        let session_id = session.id.clone();
                        session.tagline = Some(new_tagline.clone());
                        return Some(Action::SetTagline {
                            session_id,
                            tagline: new_tagline,
                        });
                    }
                    return None;
                }
                KeyCode::Backspace => {
                    if *cursor_pos > 0 {
                        // Find the previous char boundary
                        let prev = text[..*cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        text.drain(prev..*cursor_pos);
                        *cursor_pos = prev;
                    }
                    return None;
                }
                KeyCode::Delete => {
                    if *cursor_pos < text.len() {
                        // Find the next char boundary
                        let next = text[*cursor_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| *cursor_pos + i)
                            .unwrap_or(text.len());
                        text.drain(*cursor_pos..next);
                    }
                    return None;
                }
                KeyCode::Left => {
                    if *cursor_pos > 0 {
                        // Move to previous char boundary
                        *cursor_pos = text[..*cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                    return None;
                }
                KeyCode::Right => {
                    if *cursor_pos < text.len() {
                        // Move to next char boundary
                        *cursor_pos = text[*cursor_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| *cursor_pos + i)
                            .unwrap_or(text.len());
                    }
                    return None;
                }
                KeyCode::Home => {
                    *cursor_pos = 0;
                    return None;
                }
                KeyCode::End => {
                    *cursor_pos = text.len();
                    return None;
                }
                KeyCode::Char(c) => {
                    text.insert(*cursor_pos, c);
                    *cursor_pos += c.len_utf8();
                    return None;
                }
                _ => return None,
            }
        }

        // If in filter input mode, handle keys for text editing
        if self.picker_filter_mode {
            match key.code {
                KeyCode::Esc => {
                    // Clear filter and exit filter mode
                    self.picker_filter.clear();
                    self.picker_filter_mode = false;
                    self.picker_cursor = 0;
                    return None;
                }
                KeyCode::Enter => {
                    // Exit filter mode, keep filter text
                    self.picker_filter_mode = false;
                    self.picker_clamp_cursor();
                    return None;
                }
                KeyCode::Backspace => {
                    self.picker_filter.pop();
                    self.picker_cursor = 0;
                    return None;
                }
                KeyCode::Char(c) => {
                    self.picker_filter.push(c);
                    self.picker_cursor = 0;
                    return None;
                }
                _ => return None,
            }
        }

        // If waiting for delete confirmation
        if let Some(cursor_pos) = self.picker_confirm_delete {
            let real_idx = self.picker_filtered_indices().get(cursor_pos).copied();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.picker_confirm_delete = None;
                    if let Some(idx) = real_idx
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        let session_id = session.id.clone();
                        self.picker_sessions.remove(idx);
                        self.picker_clamp_cursor();
                        return Some(Action::DeleteSession(session_id));
                    }
                    None
                }
                _ => {
                    self.picker_confirm_delete = None;
                    None
                }
            }
        } else if let Some(cursor_pos) = self.picker_confirm_archive {
            let real_idx = self.picker_filtered_indices().get(cursor_pos).copied();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.picker_confirm_archive = None;
                    if let Some(idx) = real_idx
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        let session_id = session.id.clone();
                        let parent_id = session.parent_id.clone();
                        self.picker_sessions.remove(idx);
                        self.picker_clamp_cursor();
                        let switch_to = if session_id == self.session_id {
                            parent_id
                        } else {
                            None
                        };
                        return Some(Action::ArchiveSession {
                            session_id,
                            switch_to,
                        });
                    }
                    None
                }
                _ => {
                    self.picker_confirm_archive = None;
                    None
                }
            }
        } else {
            let filtered_len = self.picker_filtered_indices().len();
            match (key.code, key.modifiers) {
                // / enters filter mode
                (KeyCode::Char('/'), _) => {
                    self.picker_filter_mode = true;
                    None
                }
                // Navigate up
                (KeyCode::Up | KeyCode::Char('k'), _) => {
                    if self.picker_cursor > 0 {
                        self.picker_cursor -= 1;
                    }
                    None
                }
                // Navigate down
                (KeyCode::Down | KeyCode::Char('j'), _) => {
                    if filtered_len > 0 && self.picker_cursor < filtered_len - 1 {
                        self.picker_cursor += 1;
                    }
                    None
                }
                // Page up: jump up by a page
                (KeyCode::PageUp, _) => {
                    const PAGE_SIZE: usize = 10;
                    self.picker_cursor = self.picker_cursor.saturating_sub(PAGE_SIZE);
                    None
                }
                // Page down: jump down by a page
                (KeyCode::PageDown, _) => {
                    if filtered_len > 0 {
                        const PAGE_SIZE: usize = 10;
                        self.picker_cursor = (self.picker_cursor + PAGE_SIZE).min(filtered_len - 1);
                    }
                    None
                }
                // Home: jump to first item
                (KeyCode::Home, _) => {
                    self.picker_cursor = 0;
                    None
                }
                // End: jump to last item
                (KeyCode::End, _) => {
                    if filtered_len > 0 {
                        self.picker_cursor = filtered_len - 1;
                    }
                    None
                }
                // Enter: switch to selected session
                (KeyCode::Enter, _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        let session_id = session.id.clone();
                        self.mode = AppMode::Input;
                        self.picker_reset_transient();
                        if session_id == self.session_id {
                            return None;
                        }
                        return Some(Action::SwitchSession(session_id));
                    }
                    None
                }
                // TAB or ESC: close picker, return to previous mode
                (KeyCode::Tab | KeyCode::Esc, _) => {
                    self.mode = self.picker_previous_mode;
                    self.picker_reset_transient();
                    None
                }
                // D (shift+d): delete selected session
                (KeyCode::Char('D'), _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        if session.id == self.session_id {
                            self.messages.push(MessageItem::Status {
                                text: "cannot delete active session".into(),
                            });
                            self.mode = self.picker_previous_mode;
                            self.picker_reset_transient();
                        } else {
                            self.picker_confirm_delete = Some(self.picker_cursor);
                        }
                    }
                    None
                }
                // A (shift+a): archive selected session
                (KeyCode::Char('A'), _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        if session.id == self.session_id && session.parent_id.is_none() {
                            self.messages.push(MessageItem::Status {
                                text: "cannot archive active session".into(),
                            });
                            self.mode = self.picker_previous_mode;
                            self.picker_reset_transient();
                        } else {
                            self.picker_confirm_archive = Some(self.picker_cursor);
                        }
                    }
                    None
                }
                // r (lowercase): edit tagline of selected session
                (KeyCode::Char('r'), _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        let current = session.tagline.clone().unwrap_or_default();
                        let cursor_pos = current.len();
                        self.picker_edit_tagline = Some((self.picker_cursor, current, cursor_pos));
                    }
                    None
                }
                // f (lowercase): toggle fold/unfold of selected session's subtree
                (KeyCode::Char('f'), _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                    {
                        let sid = session.id.clone();
                        if self.picker_folded.contains(&sid) {
                            self.picker_folded.remove(&sid);
                        } else if session.child_count > 0 {
                            // Only fold if there's something to hide.
                            self.picker_folded.insert(sid);
                        }
                        // Cursor stays on the folded session (still visible);
                        // clamp defensively in case the filtered set shrinks.
                        self.picker_clamp_cursor();
                    }
                    None
                }
                // R (shift+r): restore (un-archive) selected session
                (KeyCode::Char('R'), _) => {
                    if let Some(idx) = self.picker_selected_session_idx()
                        && let Some(session) = self.picker_sessions.get(idx)
                        && session.archived
                    {
                        let session_id = session.id.clone();
                        self.mode = self.picker_previous_mode;
                        self.picker_reset_transient();
                        return Some(Action::RestoreSession { session_id });
                    }
                    None
                }
                // P (shift+p): toggle between project-filtered and all-sessions view
                (KeyCode::Char('P'), _) => {
                    self.picker_show_all_projects = !self.picker_show_all_projects;
                    self.picker_cursor = 0;
                    None
                }
                // Ctrl+C: close picker, return to previous mode
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.mode = self.picker_previous_mode;
                    self.picker_reset_transient();
                    None
                }
                _ => None,
            }
        }
    }

    // ---- Task picker helpers ----

    /// Return indices into `picker_tasks` that match the current task picker filter.
    /// If the filter is empty, all indices are returned.
    pub fn task_picker_filtered_indices(&self) -> Vec<usize> {
        let needle = self.task_picker_filter.to_lowercase();
        let filter_empty = self.task_picker_filter.is_empty();

        self.picker_tasks
            .iter()
            .enumerate()
            .filter(|(_, (_, t))| {
                if filter_empty {
                    return true;
                }
                // Match against task title
                if t.title.to_lowercase().contains(&needle) {
                    return true;
                }
                // Match against task ID (prefix match)
                if t.id.to_string().starts_with(&needle) {
                    return true;
                }
                // Match against state name
                if t.state.to_lowercase().contains(&needle) {
                    return true;
                }
                // Match against session_id
                if let Some(ref sid) = t.session_id {
                    if sid.to_lowercase().contains(&needle) {
                        return true;
                    }
                }
                // Match against tags
                if let Some(ref tags) = t.tags {
                    if let Some(arr) = tags.as_array() {
                        for tag in arr {
                            if let Some(s) = tag.as_str() {
                                if s.to_lowercase().contains(&needle) {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Resolve the task picker cursor to an index in `picker_tasks`.
    /// Returns `None` if no matching tasks or cursor is out of range.
    pub fn task_picker_selected_task_idx(&self) -> Option<usize> {
        let filtered = self.task_picker_filtered_indices();
        filtered.get(self.task_picker_cursor).copied()
    }

    /// Reset transient task picker state (confirm, filter, detail, create mode).
    fn task_picker_reset_transient(&mut self) {
        self.task_picker_confirm = None;
        self.task_picker_filter.clear();
        self.task_picker_filter_mode = false;
        self.task_picker_create_mode = false;
        self.task_picker_detail = None;
    }

    /// Clamp task picker cursor to valid range within filtered results.
    fn task_picker_clamp_cursor(&mut self) {
        let filtered = self.task_picker_filtered_indices();
        if filtered.is_empty() {
            self.task_picker_cursor = 0;
        } else if self.task_picker_cursor >= filtered.len() {
            self.task_picker_cursor = filtered.len() - 1;
        }
    }

    fn handle_task_picker_key(&mut self, key: &KeyEvent) -> Option<Action> {
        // Priority 1: Task detail view
        if let Some(ref mut detail) = self.task_picker_detail {
            // If there's a pending confirmation while in detail mode, handle it first
            if let Some((_, _, action_kind)) = self.task_picker_confirm.take() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        let task_id = detail.task.id;
                        return self.execute_task_picker_confirm(task_id, action_kind);
                    }
                    _ => {
                        // Cancelled
                        return None;
                    }
                }
            }

            match key.code {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.task_picker_detail = None;
                    return None;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    // re-borrow after the outer match
                    if let Some(ref mut d) = self.task_picker_detail {
                        d.scroll = d.scroll.saturating_add(1);
                    }
                    return None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(ref mut d) = self.task_picker_detail {
                        d.scroll = d.scroll.saturating_sub(1);
                    }
                    return None;
                }
                KeyCode::PageDown => {
                    if let Some(ref mut d) = self.task_picker_detail {
                        d.scroll = d.scroll.saturating_add(10);
                    }
                    return None;
                }
                KeyCode::PageUp => {
                    if let Some(ref mut d) = self.task_picker_detail {
                        d.scroll = d.scroll.saturating_sub(10);
                    }
                    return None;
                }
                KeyCode::Char('a') => {
                    let task_id = self
                        .task_picker_detail
                        .as_ref()
                        .expect("detail should be Some in detail mode")
                        .task
                        .id;
                    self.task_picker_confirm = Some((
                        self.task_picker_cursor,
                        format!("Approve #{}?", task_id),
                        TaskPickerConfirmAction::Approve,
                    ));
                    return None;
                }
                KeyCode::Char('r') => {
                    let task_id = self
                        .task_picker_detail
                        .as_ref()
                        .expect("detail should be Some in detail mode")
                        .task
                        .id;
                    self.task_picker_confirm = Some((
                        self.task_picker_cursor,
                        format!("Ready #{}?", task_id),
                        TaskPickerConfirmAction::Ready,
                    ));
                    return None;
                }
                KeyCode::Char('d') => {
                    let task_id = self
                        .task_picker_detail
                        .as_ref()
                        .expect("detail should be Some in detail mode")
                        .task
                        .id;
                    self.task_picker_confirm = Some((
                        self.task_picker_cursor,
                        format!("Dispatch #{}?", task_id),
                        TaskPickerConfirmAction::Dispatch,
                    ));
                    return None;
                }
                KeyCode::Char('g') | KeyCode::Enter => {
                    let detail = self
                        .task_picker_detail
                        .as_ref()
                        .expect("detail should be Some in detail mode");
                    if let Some(session_id) = primary_session_id(&detail.task, &detail.sessions) {
                        self.mode = self.task_picker_previous_mode;
                        self.task_picker_reset_transient();
                        return Some(Action::SwitchSession(session_id));
                    }
                    return None;
                }
                _ => return None,
            }
        }

        // Priority 2: Confirmation mode
        if let Some((cursor_pos, _, action_kind)) = self.task_picker_confirm.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let filtered = self.task_picker_filtered_indices();
                    if let Some(&real_idx) = filtered.get(cursor_pos) {
                        if let Some((_, task)) = self.picker_tasks.get(real_idx) {
                            let task_id = task.id;
                            return self.execute_task_picker_confirm(task_id, action_kind);
                        }
                    }
                    None
                }
                _ => {
                    // Cancelled
                    None
                }
            }
        }
        // Priority 3: Filter input mode
        else if self.task_picker_filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.task_picker_filter.clear();
                    self.task_picker_filter_mode = false;
                    self.task_picker_create_mode = false;
                    self.task_picker_cursor = 0;
                    None
                }
                KeyCode::Enter => {
                    if self.task_picker_create_mode && !self.task_picker_filter.is_empty() {
                        let title = self.task_picker_filter.clone();
                        let project = self.task_project();
                        self.task_picker_filter.clear();
                        self.task_picker_filter_mode = false;
                        self.task_picker_create_mode = false;
                        Some(Action::TaskCreate { project, title })
                    } else {
                        self.task_picker_filter_mode = false;
                        self.task_picker_create_mode = false;
                        self.task_picker_clamp_cursor();
                        None
                    }
                }
                KeyCode::Backspace => {
                    self.task_picker_filter.pop();
                    self.task_picker_cursor = 0;
                    None
                }
                KeyCode::Char(c) => {
                    self.task_picker_filter.push(c);
                    self.task_picker_cursor = 0;
                    None
                }
                _ => None,
            }
        }
        // Priority 4: Normal navigation
        else {
            let filtered_len = self.task_picker_filtered_indices().len();
            match (key.code, key.modifiers) {
                // / enters filter mode
                (KeyCode::Char('/'), _) => {
                    self.task_picker_filter_mode = true;
                    None
                }
                // c enters create mode (filter + create)
                (KeyCode::Char('c'), m) if !m.contains(KeyModifiers::CONTROL) => {
                    self.task_picker_filter_mode = true;
                    self.task_picker_create_mode = true;
                    None
                }
                // Navigate up
                (KeyCode::Up | KeyCode::Char('k'), _) => {
                    if self.task_picker_cursor > 0 {
                        self.task_picker_cursor -= 1;
                    }
                    None
                }
                // Navigate down
                (KeyCode::Down | KeyCode::Char('j'), _) => {
                    if filtered_len > 0 && self.task_picker_cursor < filtered_len - 1 {
                        self.task_picker_cursor += 1;
                    }
                    None
                }
                // Page up
                (KeyCode::PageUp, _) => {
                    const PAGE_SIZE: usize = 10;
                    self.task_picker_cursor = self.task_picker_cursor.saturating_sub(PAGE_SIZE);
                    None
                }
                // Page down
                (KeyCode::PageDown, _) => {
                    if filtered_len > 0 {
                        const PAGE_SIZE: usize = 10;
                        self.task_picker_cursor =
                            (self.task_picker_cursor + PAGE_SIZE).min(filtered_len - 1);
                    }
                    None
                }
                // Home: jump to first
                (KeyCode::Home, _) => {
                    self.task_picker_cursor = 0;
                    None
                }
                // End: jump to last
                (KeyCode::End, _) => {
                    if filtered_len > 0 {
                        self.task_picker_cursor = filtered_len - 1;
                    }
                    None
                }
                // Enter: fetch task detail
                (KeyCode::Enter, _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                    {
                        return Some(Action::TaskGet { id: task.id });
                    }
                    None
                }
                // a: approve selected task (with confirmation)
                (KeyCode::Char('a'), _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                    {
                        self.task_picker_confirm = Some((
                            self.task_picker_cursor,
                            format!("Approve #{}?", task.id),
                            TaskPickerConfirmAction::Approve,
                        ));
                    }
                    None
                }
                // r: ready selected task (with confirmation)
                (KeyCode::Char('r'), _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                    {
                        self.task_picker_confirm = Some((
                            self.task_picker_cursor,
                            format!("Ready #{}?", task.id),
                            TaskPickerConfirmAction::Ready,
                        ));
                    }
                    None
                }
                // d: dispatch selected task (with confirmation)
                (KeyCode::Char('d'), _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                    {
                        self.task_picker_confirm = Some((
                            self.task_picker_cursor,
                            format!("Dispatch #{}?", task.id),
                            TaskPickerConfirmAction::Dispatch,
                        ));
                    }
                    None
                }
                // s: schedule
                (KeyCode::Char('s'), _) => {
                    let project = self.task_project();
                    Some(Action::TaskSchedule { project })
                }
                // m: merge selected task (with confirmation)
                (KeyCode::Char('m'), _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                    {
                        self.task_picker_confirm = Some((
                            self.task_picker_cursor,
                            format!("Merge #{}?", task.id),
                            TaskPickerConfirmAction::Merge,
                        ));
                    }
                    None
                }
                // g: go to session
                (KeyCode::Char('g'), _) => {
                    if let Some(idx) = self.task_picker_selected_task_idx()
                        && let Some((_, task)) = self.picker_tasks.get(idx)
                        && let Some(ref session_id) = task.session_id
                    {
                        let session_id = session_id.clone();
                        self.mode = self.task_picker_previous_mode;
                        self.task_picker_reset_transient();
                        return Some(Action::SwitchSession(session_id));
                    }
                    None
                }
                // F2/Esc: close picker, restore previous mode
                (KeyCode::F(2) | KeyCode::Esc, _) => {
                    self.mode = self.task_picker_previous_mode;
                    self.task_picker_reset_transient();
                    None
                }
                // Ctrl+C: close picker
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.mode = self.task_picker_previous_mode;
                    self.task_picker_reset_transient();
                    None
                }
                _ => None,
            }
        }
    }

    /// Execute a confirmed task picker action.
    fn execute_task_picker_confirm(
        &self,
        task_id: i64,
        action: TaskPickerConfirmAction,
    ) -> Option<Action> {
        match action {
            TaskPickerConfirmAction::Approve => Some(Action::TaskUpdate {
                id: task_id,
                state: "approved".into(),
            }),
            TaskPickerConfirmAction::Ready => Some(Action::TaskUpdate {
                id: task_id,
                state: "ready".into(),
            }),
            TaskPickerConfirmAction::Dispatch => Some(Action::TaskDispatch { id: task_id }),
            TaskPickerConfirmAction::Merge => Some(Action::TaskMerge { id: task_id }),
        }
    }

    fn handle_streaming_key(&mut self, key: &KeyEvent) -> Option<Action> {
        match (key.code, key.modifiers) {
            // Enter during streaming: inject steering message into agent loop
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let text: String = self.textarea.lines().join("\n");
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.textarea.select_all();
                self.textarea.cut();

                // Handle slash commands immediately, don't queue them
                if text.starts_with('/') {
                    self.push_input_history(&text);
                    self.history_index = None;
                    return self.handle_slash_command(&text);
                }

                self.push_input_history(&text);
                self.scroll_to_bottom();
                self.history_index = None;
                self.pending_steer = Some(text.clone());
                Some(Action::Steer(text))
            }
            // Alt+Enter during streaming: queue message on the server (sent after current turn)
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
                let text: String = self.textarea.lines().join("\n");
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.textarea.select_all();
                self.textarea.cut();

                // Handle slash commands immediately, don't queue them
                if text.starts_with('/') {
                    self.push_input_history(&text);
                    self.history_index = None;
                    return self.handle_slash_command(&text);
                }

                // Send to server immediately as a queued message; the server
                // will process it once the current agent turn finishes.
                self.push_input_history(&text);
                self.messages.push(MessageItem::Status {
                    text: format!("[queued: {}]", text),
                });
                Some(Action::QueueMessage(text))
            }
            // Shift+Enter during streaming: insert newline in textarea
            (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => {
                self.textarea.insert_newline();
                None
            }
            // Escape: detect double-escape for cancel
            (KeyCode::Esc, _) => {
                let now = std::time::Instant::now();
                if now.duration_since(self.last_escape) < std::time::Duration::from_millis(500) {
                    self.messages.push(MessageItem::Status {
                        text: "[cancelling...]".into(),
                    });
                    return Some(Action::CancelChat);
                }
                self.last_escape = now;
                None
            }
            // Ctrl+C during streaming: cancel
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.messages.push(MessageItem::Status {
                    text: "[cancelling...]".into(),
                });
                Some(Action::CancelChat)
            }
            // Ctrl+D during streaming: quit
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            // Page up/down still works during streaming
            (KeyCode::PageUp, _) => {
                self.scroll_up(10);
                None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_down(10);
                None
            }
            // Home/End for scroll during streaming
            (KeyCode::Home, KeyModifiers::NONE) => {
                self.scroll_to_top();
                None
            }
            (KeyCode::End, KeyModifiers::NONE) => {
                self.scroll_to_bottom();
                None
            }
            // F3: toggle expand/collapse all tool outputs
            (KeyCode::F(3), _) => {
                self.all_tools_expanded = !self.all_tools_expanded;
                for item in &mut self.messages {
                    if let MessageItem::ToolComplete { expanded, .. } = item {
                        *expanded = self.all_tools_expanded;
                    }
                }
                None
            }
            // TAB: open session picker
            (KeyCode::Tab, _) => Some(Action::OpenSessionPicker),
            // F2: open task picker
            (KeyCode::F(2), _) => Some(Action::OpenTaskPicker),
            // All other keys go to textarea (compose steering message while streaming)
            _ => {
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
        }
    }

    /// Transition mode safely: if the session picker is open, update
    /// `picker_previous_mode` instead so the picker stays visible and the
    /// correct mode is restored when the picker closes.
    fn set_mode(&mut self, target: AppMode) {
        if self.mode == AppMode::SessionPicker {
            self.picker_previous_mode = target;
        } else if self.mode == AppMode::TaskPicker {
            self.task_picker_previous_mode = target;
        } else {
            self.mode = target;
        }
    }

    /// Save current session state to navigation stack.
    pub fn save_nav_state(&mut self) {
        self.nav_stack.push(NavEntry {
            session_id: self.session_id.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            messages: std::mem::take(&mut self.messages),
            totals: std::mem::take(&mut self.totals),
            parent_id: self.parent_id.clone(),
            child_count: self.child_count,
            subscription_usage: self.subscription_usage.take(),
            last_usage_fetch: self.last_usage_fetch,
            session_cwd: self.session_cwd.clone(),
            session_project_name: self.session_project_name.clone(),
        });
    }

    /// Switch to a new session, replacing current state.
    /// Call `save_nav_state()` first if you want to preserve the current session.
    pub fn switch_to_session(
        &mut self,
        info: &tau_agent::protocol::SessionInfo,
        messages: Vec<Message>,
    ) {
        self.session_id = info.id.clone();
        self.model = info.model.clone();
        self.provider = info.provider.clone();
        self.parent_id = info.parent_id.clone();
        self.child_count = info.child_count;
        self.session_cwd = info.cwd.clone();
        self.session_project_name = info.project_name.clone();
        self.current_task_id = None;
        self.totals = UsageTotals::default();
        self.totals.context_window = info.stats.context_window;
        self.totals.is_subscription = info.stats.is_subscription;
        self.messages.clear();
        self.restore_messages(&messages);
        self.scroll_to_bottom();
        self.mode = AppMode::Input;
        self.pending_steer = None;
    }

    /// Navigate back to the previous session from the nav stack.
    pub fn navigate_back(&mut self) -> bool {
        if let Some(entry) = self.nav_stack.pop() {
            self.session_id = entry.session_id;
            self.model = entry.model;
            self.provider = entry.provider;
            self.messages = entry.messages;
            self.totals = entry.totals;
            self.parent_id = entry.parent_id;
            self.child_count = entry.child_count;
            self.subscription_usage = entry.subscription_usage;
            self.last_usage_fetch = entry.last_usage_fetch;
            self.session_cwd = entry.session_cwd;
            self.session_project_name = entry.session_project_name;
            self.current_task_id = None;
            self.scroll_to_bottom();
            self.mode = AppMode::Input;
            self.pending_steer = None;
            true
        } else {
            false
        }
    }

    fn handle_slash_command(&mut self, text: &str) -> Option<Action> {
        let (cmd, args) = text.split_once(' ').unwrap_or((text, ""));
        let args = args.trim();

        match cmd {
            "/quit" | "/exit" => {
                self.should_quit = true;
                None
            }
            "/model" | "/models" => {
                if args.is_empty() {
                    Some(Action::ListModels)
                } else {
                    let parts: Vec<&str> = args.splitn(2, ' ').collect();
                    let model_id = parts[0].to_string();
                    let set_default = parts.get(1).is_some_and(|s| s.trim() == "default");
                    if set_default {
                        let mut s = crate::settings::load();
                        s.tui.model = Some(model_id.clone());
                        crate::settings::save(&s);
                        self.messages.push(MessageItem::Status {
                            text: format!("default model: {}", model_id),
                        });
                    }
                    Some(Action::SetModel(model_id))
                }
            }
            "/status" => Some(Action::GetStatus),
            "/theme" | "/themes" => {
                if args.is_empty() {
                    // List available themes
                    let themes = crate::theme::list_themes();
                    for name in &themes {
                        let marker = if self.theme.name.as_deref() == Some(name.as_str()) {
                            " *"
                        } else {
                            ""
                        };
                        self.messages.push(MessageItem::Status {
                            text: format!("  {}{}", name, marker),
                        });
                    }
                } else {
                    // Switch theme and persist
                    match crate::theme::load_by_name(args) {
                        Ok(new_theme) => {
                            self.theme = new_theme;
                            let mut s = crate::settings::load();
                            s.tui.theme = Some(args.to_string());
                            crate::settings::save(&s);
                            self.messages.push(MessageItem::Status {
                                text: format!("theme: {}", args),
                            });
                        }
                        Err(e) => {
                            self.messages.push(MessageItem::Error { text: e });
                        }
                    }
                }
                None
            }
            "/sessions" | "/children" => Some(Action::ListChildren),
            "/session" => {
                if args.is_empty() {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /session <id>".into(),
                    });
                    None
                } else {
                    Some(Action::SwitchSession(args.to_string()))
                }
            }
            "/back" | "/up" => {
                if !self.nav_stack.is_empty() {
                    Some(Action::NavigateBack)
                } else if let Some(pid) = self.parent_id.clone() {
                    Some(Action::SwitchSession(pid))
                } else {
                    self.messages.push(MessageItem::Error {
                        text: "no parent session".into(),
                    });
                    None
                }
            }
            "/archive" => {
                let current = self.session_id.clone();
                // Determine the session to switch to after archiving
                let switch_to = self
                    .nav_stack
                    .last()
                    .map(|entry| entry.session_id.clone())
                    .or_else(|| self.parent_id.clone());
                if switch_to.is_some() {
                    Some(Action::ArchiveSession {
                        session_id: current,
                        switch_to,
                    })
                } else {
                    self.messages.push(MessageItem::Error {
                        text: "no previous session to switch to".into(),
                    });
                    None
                }
            }
            "/reload" => Some(Action::ReloadPlugins),
            "/fork" => Some(Action::ForkSession),
            "/new" => Some(Action::NewSession),
            "/help" => {
                self.messages.push(MessageItem::Status {
                    text: "Commands: /status /model [id] /theme [name] /cwd [path] /task [list|get|create|search|claim|approve|ready|status|mq] /reload /sessions /session <id> /back /fork /new /archive /help /quit"
                        .into(),
                });
                None
            }
            "/cwd" => {
                if args.is_empty() {
                    Some(Action::GetStatus)
                } else {
                    Some(Action::SetCwd(args.to_string()))
                }
            }
            "/task" | "/tasks" => self.handle_task_slash_command(args),
            _ => {
                self.messages.push(MessageItem::Error {
                    text: format!("unknown command: {}. Type /help", cmd),
                });
                None
            }
        }
    }

    fn handle_task_slash_command(&mut self, args: &str) -> Option<Action> {
        let parts: Vec<&str> = args.splitn(3, ' ').collect();
        let subcmd = parts.first().copied().unwrap_or("");

        match subcmd {
            "" | "list" => {
                let state_filter = if parts.len() > 1 {
                    Some(parts[1].to_string())
                } else {
                    None
                };
                Some(Action::TaskList {
                    project: self.task_project(),
                    state: state_filter,
                })
            }
            "get" => {
                let Some(id_str) = parts.get(1) else {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task get <id>".into(),
                    });
                    return None;
                };
                let Ok(id) = id_str.parse::<i64>() else {
                    self.messages.push(MessageItem::Error {
                        text: format!("invalid task id: {}", id_str),
                    });
                    return None;
                };
                Some(Action::TaskGet { id })
            }
            "approve" => {
                let Some(id_str) = parts.get(1) else {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task approve <id>".into(),
                    });
                    return None;
                };
                let Ok(id) = id_str.parse::<i64>() else {
                    self.messages.push(MessageItem::Error {
                        text: format!("invalid task id: {}", id_str),
                    });
                    return None;
                };
                Some(Action::TaskUpdate {
                    id,
                    state: "approved".into(),
                })
            }
            "ready" => {
                let Some(id_str) = parts.get(1) else {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task ready <id>".into(),
                    });
                    return None;
                };
                let Ok(id) = id_str.parse::<i64>() else {
                    self.messages.push(MessageItem::Error {
                        text: format!("invalid task id: {}", id_str),
                    });
                    return None;
                };
                Some(Action::TaskUpdate {
                    id,
                    state: "ready".into(),
                })
            }
            "create" => {
                let title = args.strip_prefix("create").unwrap_or("").trim();
                if title.is_empty() {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task create <title>".into(),
                    });
                    return None;
                }
                Some(Action::TaskCreate {
                    project: self.task_project(),
                    title: title.to_string(),
                })
            }
            "search" => {
                let query = args.strip_prefix("search").unwrap_or("").trim();
                if query.is_empty() {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task search <query>".into(),
                    });
                    return None;
                }
                Some(Action::TaskSearch {
                    project: self.task_project(),
                    query: query.to_string(),
                })
            }
            "claim" => {
                let Some(id_str) = parts.get(1) else {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task claim <id>".into(),
                    });
                    return None;
                };
                let Ok(id) = id_str.parse::<i64>() else {
                    self.messages.push(MessageItem::Error {
                        text: format!("invalid task id: {}", id_str),
                    });
                    return None;
                };
                Some(Action::TaskAssign {
                    id,
                    session_id: self.session_id.clone(),
                })
            }
            "status" | "queue" => Some(Action::TaskStatus {
                project: self.task_project(),
            }),
            "mq" => Some(Action::TaskMergeQueue {
                project: self.task_project(),
            }),
            "switch" => {
                let Some(id_str) = parts.get(1) else {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task switch <id>".into(),
                    });
                    return None;
                };
                let Ok(id) = id_str.parse::<i64>() else {
                    self.messages.push(MessageItem::Error {
                        text: format!("invalid task id: {}", id_str),
                    });
                    return None;
                };
                // Ask the server for the task detail; the response handler
                // resolves the primary session and emits SwitchSession.
                self.pending_task_switch = Some(id);
                Some(Action::TaskGet { id })
            }
            "active" => {
                // Open the task picker pre-filtered to active tasks so the
                // user can j/k to pick and Enter to switch.
                Some(Action::OpenTaskPickerWithState {
                    state: "active".into(),
                })
            }
            _ => {
                self.messages.push(MessageItem::Error {
                    text: format!(
                        "unknown task command: {}. Use: list [state], get <id>, create <title>, search <query>, claim <id>, approve <id>, ready <id>, switch <id>, active, status, mq",
                        subcmd
                    ),
                });
                None
            }
        }
    }

    /// Get the project path for task DB queries.
    /// Uses the current session's cwd if available, falls back to process cwd.
    pub fn task_project(&self) -> String {
        if let Some(ref name) = self.session_project_name {
            return name.clone();
        }
        // Fallback: discover project name from session cwd or process cwd.
        let cwd = self
            .session_cwd
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        if let Some(cwd) = cwd {
            if let Some((name, _root)) = tau_agent::project::discover_project(cwd.as_path()) {
                return name;
            }
        }
        // Last resort: return cwd as-is (will likely match nothing).
        self.session_cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
    }

    /// Render a flat task list (search results, merge queue).
    fn render_task_list_flat(&mut self, tasks: &[TaskInfo]) {
        if tasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "no matching tasks".into(),
            });
            return;
        }
        self.messages.push(MessageItem::Status {
            text: format!(
                "  {:>4}  {:<12}  {:>8}  {:<8}  TITLE",
                "ID", "STATE", "PRIORITY", "SESSION"
            ),
        });
        for t in tasks {
            let session = t.session_id.as_deref().unwrap_or("-");
            let text = format!(
                "  {:>4}  {:<12}  {:>8}  {:<8}  {}",
                t.id, t.state, t.priority, session, t.title
            );
            if t.state == "failed" {
                self.messages.push(MessageItem::Error { text });
            } else {
                self.messages.push(MessageItem::Status { text });
            }
        }
    }

    /// Render task detail (task get response).
    fn render_task_detail(
        &mut self,
        task: &TaskInfo,
        messages: &[TaskMessageInfo],
        subtasks: &[TaskInfo],
        sessions: &[TaskSessionInfo],
        history: &[TaskHistoryInfo],
    ) {
        let skip = if task.skip_review { "yes" } else { "no" };
        let held = if task.held { "yes \u{1F512}" } else { "no" };
        let branch = task.branch.as_deref().unwrap_or("none");
        let parent = task
            .parent_id
            .map(|p| format!("#{}", p))
            .unwrap_or_else(|| "none".to_string());

        self.messages.push(MessageItem::Status {
            text: format!("Task #{}: {}", task.id, task.title),
        });
        self.messages.push(MessageItem::Status {
            text: format!(
                "  State: {:<12} Priority: {:<3} Skip review: {}",
                task.state, task.priority, skip
            ),
        });
        self.messages.push(MessageItem::Status {
            text: format!(
                "  Branch: {:<16} Parent: {:<8} Held: {}",
                branch, parent, held
            ),
        });

        // Sessions block.
        if !sessions.is_empty() {
            let now = now_secs();
            let live_count = sessions.iter().filter(|s| s.is_live).count();
            let total = sessions.len();
            let badge = if live_count > 0 {
                format!("Sessions ({}, {} live)", total, live_count)
            } else {
                format!("Sessions ({})", total)
            };
            self.messages.push(MessageItem::Status { text: badge });
            for s in sessions {
                let msgs = s
                    .message_count
                    .map(|n| format!("{} msgs", n))
                    .unwrap_or_else(|| "-".to_string());
                let phase_or_exit = if s.is_live {
                    s.last_phase
                        .as_deref()
                        .map(|p| format!("live({})", p))
                        .unwrap_or_else(|| "live".to_string())
                } else if s.archived == Some(true) {
                    "archived".to_string()
                } else if let Some(ref ex) = s.last_exit_status {
                    format!("idle ({})", ex)
                } else {
                    "idle".to_string()
                };
                let age = s
                    .last_activity
                    .map(|t| format_age_since(now, t))
                    .unwrap_or_else(|| "-".to_string());
                let text = format!(
                    "  {:<10} {:<7} {:<20} {:<10} last: {}",
                    s.role, s.session_id, phase_or_exit, msgs, age
                );
                self.messages.push(MessageItem::Status { text });
            }
        }

        // History block — reverse-chronological (most recent first), capped
        // at 10 visible entries.
        if !history.is_empty() {
            let total = history.len();
            let shown = total.min(10);
            let header = if total > shown {
                format!("History ({} entries, last {})", total, shown)
            } else {
                format!("History ({} entries)", total)
            };
            self.messages.push(MessageItem::Status { text: header });
            let now_ms = now_secs() * 1000;
            // History is chronological; walk in reverse.
            for entry in history.iter().rev().take(shown) {
                let age = format_age_since_ms(now_ms, entry.created_at);
                let body = render_history_entry(entry);
                let sid = entry
                    .session_id
                    .as_deref()
                    .map(|s| format!(" ({})", s))
                    .unwrap_or_default();
                self.messages.push(MessageItem::Status {
                    text: format!("  {:<6} {}{}", age, body, sid),
                });
            }
        }

        if !messages.is_empty() {
            self.messages.push(MessageItem::Status {
                text: format!("Messages: {}", messages.len()),
            });
            for msg in messages {
                let author = msg.author.as_deref().unwrap_or("unknown");
                let preview: String = msg.content.chars().take(80).collect();
                let ellipsis = if msg.content.len() > 80 { "..." } else { "" };
                self.messages.push(MessageItem::Status {
                    text: format!("  #{} [{}] {}{}", msg.id, author, preview, ellipsis),
                });
            }
        }

        if !subtasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "Subtasks:".into(),
            });
            for st in subtasks {
                let text = format!("  #{:<4} {:<8} {}", st.id, st.state, st.title);
                if st.state == "failed" {
                    self.messages.push(MessageItem::Error { text });
                } else {
                    self.messages.push(MessageItem::Status { text });
                }
            }
        }

        // Footer hint for quick session switch.
        if let Some(primary) = primary_session_id(task, sessions) {
            self.messages.push(MessageItem::Status {
                text: format!(
                    "  Use `/task switch {}` to jump to session {}.",
                    task.id, primary
                ),
            });
        }
    }

    /// Render a merge queue (approved + merging tasks).
    fn render_merge_queue(&mut self, tasks: &[TaskInfo]) {
        if tasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "merge queue is empty".into(),
            });
            return;
        }
        self.messages.push(MessageItem::Status {
            text: "  MERGE QUEUE".into(),
        });
        self.messages.push(MessageItem::Status {
            text: format!("  {:>4}  {:<12}  {:<14}  TITLE", "ID", "STATE", "BRANCH"),
        });
        for t in tasks {
            let branch = t.branch.as_deref().unwrap_or("-");
            self.messages.push(MessageItem::Status {
                text: format!(
                    "  {:>4}  {:<12}  {:<14}  {}",
                    t.id, t.state, branch, t.title
                ),
            });
        }
    }

    /// Remove empty AssistantStreaming placeholder if present.
    fn cleanup_empty_streaming(&mut self) {
        if let Some(MessageItem::AssistantStreaming { text }) = self.messages.last()
            && text.is_empty()
        {
            self.messages.pop();
        }
    }

    /// Finalize all in-flight display items after an interruption (cancel or
    /// abrupt agent-done).  Walks the message list and converts any
    /// still-active items to their completed equivalents so the TUI does not
    /// keep showing spinners/cursors after the agent loop has ended.
    fn finalize_in_flight(&mut self) {
        for item in self.messages.iter_mut() {
            match item {
                MessageItem::AssistantStreaming { text } => {
                    if text.is_empty() {
                        // Will be removed by the cleanup pass below.
                        continue;
                    }
                    *item = MessageItem::Assistant {
                        text: std::mem::take(text),
                    };
                }
                MessageItem::Thinking { done, .. } if !*done => {
                    *done = true;
                }
                MessageItem::ToolActive {
                    name,
                    args,
                    started_at,
                    ..
                } => {
                    *item = MessageItem::ToolComplete {
                        name: std::mem::take(name),
                        args: std::mem::take(args),
                        output: "[interrupted]".into(),
                        is_error: true,
                        duration: Some(started_at.elapsed()),
                        summary: None,
                        expanded: self.all_tools_expanded,
                    };
                }
                _ => {}
            }
        }
        // Remove any remaining empty streaming placeholders.
        self.messages
            .retain(|m| !matches!(m, MessageItem::AssistantStreaming { text } if text.is_empty()));
    }

    /// Derive phase from stream events that implicitly indicate a transition.
    /// Called from handle_stream_event; avoids sending redundant Phase messages
    /// from the server for events that already carry enough information.
    fn update_phase_from_event(&mut self, event: &StreamEvent) {
        match event {
            // Thinking tokens → Thinking phase
            StreamEvent::ThinkingStart { .. } | StreamEvent::ThinkingDelta { .. } => {
                self.phase = AgentPhase::Thinking;
            }
            // Text/toolcall tokens → Responding phase
            StreamEvent::TextStart { .. }
            | StreamEvent::TextDelta { .. }
            | StreamEvent::ToolcallStart { .. } => {
                self.phase = AgentPhase::Responding;
            }
            // Tool call defined or result received → ToolExec phase
            StreamEvent::ToolcallEnd { .. } | StreamEvent::ToolResult { .. } => {
                self.phase = AgentPhase::ToolExec;
            }
            // Explicit phase transition
            StreamEvent::Phase { phase } => {
                self.phase = *phase;
            }
            _ => {}
        }
    }

    fn handle_server_response(&mut self, response: Response) -> Option<Action> {
        match response {
            Response::Stream { event } => {
                // Phase(Idle) means no active agent — if we're in Streaming
                // mode (e.g. after a subscribe reconnect that missed AgentDone),
                // transition back to Input.
                if let StreamEvent::Phase {
                    phase: AgentPhase::Idle,
                } = *event
                {
                    let effective = if self.mode == AppMode::SessionPicker {
                        self.picker_previous_mode
                    } else if self.mode == AppMode::TaskPicker {
                        self.task_picker_previous_mode
                    } else {
                        self.mode
                    };
                    if effective == AppMode::Streaming {
                        self.finalize_in_flight();
                        self.set_mode(AppMode::Input);
                    }
                    self.phase = AgentPhase::Idle;
                    return None;
                }
                // If we receive stream events while in Input mode,
                // another client is chatting — switch to streaming view.
                let effective = if self.mode == AppMode::SessionPicker {
                    self.picker_previous_mode
                } else if self.mode == AppMode::TaskPicker {
                    self.task_picker_previous_mode
                } else {
                    self.mode
                };
                if effective == AppMode::Input {
                    self.set_mode(AppMode::Streaming);
                }
                self.handle_stream_event(*event);
            }
            Response::AgentDone => {
                self.finalize_in_flight();
                self.phase = AgentPhase::Idle;
                self.set_mode(AppMode::Input);
                self.pending_steer = None;
            }
            Response::Cancelled => {
                self.finalize_in_flight();
                // Replace "cancelling" status with "cancelled"
                if let Some(last) = self.messages.last_mut()
                    && matches!(last, MessageItem::Status { text } if text.contains("cancelling"))
                {
                    *last = MessageItem::Status {
                        text: "[cancelled]".into(),
                    };
                } else {
                    self.messages.push(MessageItem::Status {
                        text: "[cancelled]".into(),
                    });
                }
                self.phase = AgentPhase::Idle;
                self.set_mode(AppMode::Input);
                self.pending_steer = None;
            }
            Response::ServerShutdown { restart } => {
                if restart {
                    self.messages.push(MessageItem::Status {
                        text: "[server restarting...]".into(),
                    });
                } else {
                    self.messages.push(MessageItem::Status {
                        text: "[server shutting down]".into(),
                    });
                    self.should_quit = true;
                }
            }
            Response::Error { message } => {
                self.finalize_in_flight();
                self.messages.push(MessageItem::Error { text: message });
                self.phase = AgentPhase::Idle;
                self.set_mode(AppMode::Input);
                self.pending_steer = None;
            }
            Response::UserMessage { text } => {
                // Another client sent a message — display it
                self.messages.push(MessageItem::User { text: text.clone() });
                // Don't reset scroll if user has scrolled up to read history
            }
            Response::Models { models } => {
                for m in &models {
                    let marker = if m.id == self.model { " *" } else { "" };
                    self.messages.push(MessageItem::Status {
                        text: format!(
                            "  {}{}\t{}\t{}K ctx",
                            m.id,
                            marker,
                            m.provider,
                            m.context_window / 1000
                        ),
                    });
                }
            }
            Response::ModelChanged { model } => {
                self.model = model.id.clone();
                self.provider = model.provider.clone();
                self.messages.push(MessageItem::Status {
                    text: format!("model changed to {}", model.id),
                });
            }
            Response::SessionInfo { info } => {
                let stats_str = tau_agent::protocol::format_stats(&info.stats);
                self.messages.push(MessageItem::Status {
                    text: format!(
                        "session: {} | {}/{} | {} msgs | {}",
                        info.id, info.provider, info.model, info.message_count, stats_str
                    ),
                });
                // Queue subscription usage fetch if applicable
                if info.stats.is_subscription {
                    self.pending_subscription_usage = true;
                }
            }
            Response::SubscriptionUsage { usage } => {
                self.subscription_usage = Some(usage);
                self.last_usage_fetch = std::time::Instant::now();
            }
            Response::Sessions { sessions } => {
                // If we're in picker mode, populate picker sessions.
                if self.mode == AppMode::SessionPicker {
                    // Sort sessions into tree order (parents before children, siblings by last_activity)
                    self.picker_sessions = tree_sort_sessions(sessions);
                    // Reset cursor -- find current session in filtered view
                    let filtered = self.picker_filtered_indices();
                    self.picker_cursor = filtered
                        .iter()
                        .position(|&i| self.picker_sessions[i].id == self.session_id)
                        .unwrap_or(0);
                    self.picker_confirm_delete = None;
                    self.picker_confirm_archive = None;
                    self.picker_edit_tagline = None;
                    // Garbage-collect fold entries for sessions that no
                    // longer exist (e.g. archived/deleted between opens).
                    // Fold state itself persists across picker open/close.
                    if !self.picker_folded.is_empty() {
                        let existing: std::collections::HashSet<&str> =
                            self.picker_sessions.iter().map(|s| s.id.as_str()).collect();
                        self.picker_folded
                            .retain(|id| existing.contains(id.as_str()));
                    }
                    return None;
                }

                // Display child sessions of the current session
                let children: Vec<_> = sessions
                    .iter()
                    .filter(|s| s.parent_id.as_deref() == Some(&self.session_id))
                    .collect();
                if children.is_empty() {
                    self.messages.push(MessageItem::Status {
                        text: "no child sessions".into(),
                    });
                } else {
                    self.messages.push(MessageItem::Status {
                        text: format!("{} child session(s):", children.len()),
                    });
                    for s in &children {
                        let stats = tau_agent::protocol::format_stats(&s.stats);
                        self.messages.push(MessageItem::Status {
                            text: format!(
                                "  {}  {}/{}  {} msgs  {}",
                                s.id, s.provider, s.model, s.message_count, stats
                            ),
                        });
                    }
                }
                if let Some(pid) = &self.parent_id {
                    self.messages.push(MessageItem::Status {
                        text: format!("parent: {}", pid),
                    });
                }
                // Update child count from fresh data
                self.child_count = children.len();
            }
            Response::TaskTree { tasks } => {
                // Scan for a task assigned to the current session (piggyback discovery)
                for (_depth, t) in &tasks {
                    if t.session_id.as_deref() == Some(&self.session_id) {
                        self.current_task_id = Some((t.id, t.title.clone()));
                        break;
                    }
                }
                // If in task picker mode, populate picker state
                if self.mode == AppMode::TaskPicker {
                    self.picker_tasks = tasks;
                    self.task_picker_cursor = 0;
                    self.task_picker_confirm = None;
                    return None;
                }
                if tasks.is_empty() {
                    self.messages.push(MessageItem::Status {
                        text: "no tasks".into(),
                    });
                    return None;
                }
                self.messages.push(MessageItem::Status {
                    text: format!(
                        "  {:>4}  {:<12}  {:>8}  {:<8}  TITLE",
                        "ID", "STATE", "PRIORITY", "SESSION"
                    ),
                });
                for (depth, t) in &tasks {
                    let session = t.session_id.as_deref().unwrap_or("-");
                    let indent = "  ".repeat(*depth);
                    let text = format!(
                        "  {:>4}  {:<12}  {:>8}  {:<8}  {}{}",
                        t.id, t.state, t.priority, session, indent, t.title
                    );
                    if t.state == "failed" {
                        self.messages.push(MessageItem::Error { text });
                    } else {
                        self.messages.push(MessageItem::Status { text });
                    }
                }
            }
            Response::TaskDetail {
                task,
                messages,
                relations,
                subtasks,
                sessions,
                history,
            } => {
                // Pending /task switch <id>: resolve primary session and
                // jump.  Consume the flag regardless so a missing session
                // doesn't leave us stuck in pending state.
                if let Some(pending_id) = self.pending_task_switch.take()
                    && pending_id == task.id
                {
                    if let Some(sid) = primary_session_id(&task, &sessions) {
                        return Some(Action::SwitchSession(sid));
                    }
                    self.messages.push(MessageItem::Error {
                        text: format!("task #{} has no session to switch to", task.id),
                    });
                    return None;
                }
                // If in task picker mode, populate detail view
                if self.mode == AppMode::TaskPicker {
                    self.task_picker_detail = Some(Box::new(TaskPickerDetail {
                        task,
                        messages,
                        relations,
                        subtasks,
                        sessions,
                        history,
                        scroll: 0,
                    }));
                    return None;
                }
                self.render_task_detail(&task, &messages, &subtasks, &sessions, &history);
            }
            Response::TaskUpdated { task } => {
                let state = task.state.clone();
                let id = task.id;
                self.messages.push(MessageItem::Status {
                    text: format!("task #{} → {} : {}", task.id, task.state, task.title),
                });
                // Track current_task_id for footer indicator
                if task.session_id.as_deref() == Some(&self.session_id) {
                    self.current_task_id = Some((task.id, task.title.clone()));
                }
                // If in task picker mode, re-fetch task list to refresh
                if self.mode == AppMode::TaskPicker {
                    return Some(Action::TaskList {
                        project: self.task_project(),
                        state: None,
                    });
                }
                // Fire hook for state changes that need scheduler notification
                if state == "approved" || state == "ready" {
                    return Some(Action::FireHook {
                        name: "task_state_changed".into(),
                        data: serde_json::json!({"task_id": id, "new_state": state}),
                    });
                }
            }
            Response::ToolExecuted { content, is_error } => {
                if is_error {
                    self.messages.push(MessageItem::Error { text: content });
                } else {
                    self.messages.push(MessageItem::Status { text: content });
                }
                // If in task picker mode, re-fetch task list after tool execution
                if self.mode == AppMode::TaskPicker {
                    return Some(Action::TaskList {
                        project: self.task_project(),
                        state: None,
                    });
                }
            }
            Response::TaskStatus { text } => {
                self.messages.push(MessageItem::Status { text });
            }
            Response::TaskList { tasks } => {
                self.render_task_list_flat(&tasks);
            }
            Response::TaskMergeQueue { tasks } => {
                self.render_merge_queue(&tasks);
            }
            _ => {}
        }
        None
    }

    fn handle_stream_event(&mut self, event: StreamEvent) {
        self.update_phase_from_event(&event);

        // Clean up empty AssistantStreaming placeholder before any non-delta event.
        if !matches!(
            event,
            StreamEvent::TextDelta { .. } | StreamEvent::TextStart { .. }
        ) {
            self.cleanup_empty_streaming();
        }

        match event {
            StreamEvent::Start { .. } => {
                self.messages.push(MessageItem::AssistantStreaming {
                    text: String::new(),
                });
                // New turn starts: clear any stale finalization flag from
                // a prior turn.  `Done` is normally responsible for
                // resetting this, but be defensive in case of
                // event-ordering quirks.
                self.turn_text_finalized = false;
                // Clear the steer indicator — the session has acknowledged
                // the steer by starting a new assistant turn.
                self.pending_steer = None;
            }
            StreamEvent::TextDelta { delta, .. } => {
                // Append to current streaming message
                if let Some(MessageItem::AssistantStreaming { text }) = self.messages.last_mut() {
                    text.push_str(&delta);
                }
                // Only auto-scroll to bottom if user hasn't scrolled up
                if !self.is_scrolled() {
                    // already at bottom, nothing to do
                }
            }
            StreamEvent::TextEnd { .. } => {
                // Convert streaming to complete
                if let Some(item) = self.messages.last_mut()
                    && let MessageItem::AssistantStreaming { text } = item
                {
                    *item = MessageItem::Assistant { text: text.clone() };
                    // Record that this turn already has a finalized text
                    // block so `Done` knows not to re-append it.
                    self.turn_text_finalized = true;
                }
            }
            StreamEvent::ThinkingStart { .. } => {
                self.messages.push(MessageItem::Thinking {
                    text: String::new(),
                    done: false,
                });
            }
            StreamEvent::ThinkingDelta { delta, .. } => {
                if let Some(MessageItem::Thinking { text, .. }) = self.messages.last_mut() {
                    text.push_str(&delta);
                }
            }
            StreamEvent::ThinkingEnd { .. } => {
                if let Some(MessageItem::Thinking { done, .. }) = self.messages.last_mut() {
                    *done = true;
                }
                // Next text content will add a new AssistantStreaming item
            }
            StreamEvent::TextStart { .. } => {
                // If last message is complete assistant or thinking, start a new streaming block
                match self.messages.last() {
                    Some(MessageItem::AssistantStreaming { .. }) => {}
                    _ => {
                        self.messages.push(MessageItem::AssistantStreaming {
                            text: String::new(),
                        });
                    }
                }
            }
            StreamEvent::ToolcallEnd { tool_call, .. } => {
                // Start active tool display
                self.messages.push(MessageItem::ToolActive {
                    tool_call_id: tool_call.id,
                    name: tool_call.name,
                    args: tool_call.arguments,
                    output_lines: Vec::new(),
                    started_at: std::time::Instant::now(),
                });
            }
            StreamEvent::ToolOutputDelta {
                tool_call_id,
                delta,
            } => {
                // Find matching active tool by tool_call_id (search from end)
                if let Some(MessageItem::ToolActive { output_lines, .. }) =
                    self.messages.iter_mut().rev().find(|m| {
                        matches!(m, MessageItem::ToolActive { tool_call_id: id, .. } if id == &tool_call_id)
                    })
                {
                    output_lines.push(delta);
                }
            }
            StreamEvent::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
                content,
                summary,
            } => {
                // Find matching active tool by tool_call_id (search from end)
                if let Some(item @ MessageItem::ToolActive { .. }) =
                    self.messages.iter_mut().rev().find(|m| {
                        matches!(m, MessageItem::ToolActive { tool_call_id: id, .. } if id == &tool_call_id)
                    })
                {
                    let (args, started_at) =
                        if let MessageItem::ToolActive {
                            args, started_at, ..
                        } = item
                        {
                            (args.clone(), *started_at)
                        } else {
                            unreachable!()
                        };
                    *item = MessageItem::ToolComplete {
                        name: tool_name,
                        args,
                        output: content,
                        is_error,
                        duration: Some(started_at.elapsed()),
                        summary,
                        expanded: self.all_tools_expanded,
                    };
                } else {
                    self.messages.push(MessageItem::ToolComplete {
                        name: tool_name,
                        args: serde_json::Value::Null,
                        output: content,
                        is_error,
                        duration: None,
                        summary,
                        expanded: self.all_tools_expanded,
                    });
                }
            }
            StreamEvent::Done { message, .. } => {
                self.totals.add(&message.usage);
                let final_text: String = message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                // Belt-and-suspenders: if the agent reports an empty
                // response with `StopReason::Error`, surface the error
                // text instead of leaving an invisible empty assistant
                // message.  This guards against any code path where the
                // server forgot to emit a separate `StreamEvent::Error`.
                let error_fallback =
                    if final_text.is_empty() && message.stop_reason == StopReason::Error {
                        Some(
                            message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "agent stopped with error".to_string()),
                        )
                    } else {
                        None
                    };

                if let Some(err_text) = error_fallback {
                    // Drop any dangling streaming placeholder, then surface
                    // the error.
                    self.finalize_in_flight();
                    self.messages.push(MessageItem::Error { text: err_text });
                } else if let Some(item) = self.messages.last_mut()
                    && let MessageItem::AssistantStreaming { .. } = item
                {
                    // Convert the in-flight streaming placeholder to its
                    // final form.
                    *item = MessageItem::Assistant { text: final_text };
                } else if !final_text.is_empty() && !self.turn_text_finalized {
                    // No streaming placeholder and `TextEnd` has not
                    // already finalized a text block for this turn (e.g.
                    // server sent `Done` without any prior text
                    // deltas) — append the final text as a fresh
                    // assistant message so it isn't lost.
                    //
                    // The `turn_text_finalized` guard fixes the
                    // regression from task #421: for a normal text-only
                    // turn, `TextEnd` converts the `AssistantStreaming`
                    // placeholder to `Assistant` *before* `Done` arrives,
                    // so without this guard the fallback branch would
                    // append a second copy of the message.
                    self.messages
                        .push(MessageItem::Assistant { text: final_text });
                }

                // Reset the per-turn finalization flag so the next turn
                // starts clean.
                self.turn_text_finalized = false;

                // Treat Done as authoritative: clear streaming mode here
                // rather than waiting for a separate `Phase::Idle` event,
                // so the TUI is robust to event-ordering quirks or a
                // dropped Idle event.
                self.phase = AgentPhase::Idle;
                self.set_mode(AppMode::Input);
            }
            StreamEvent::Error { error, .. } => {
                let msg = error
                    .error_message
                    .as_deref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        let text = error
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                AssistantContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            "unknown error".to_string()
                        } else {
                            text
                        }
                    });
                // Finalize any in-flight streaming placeholder *before*
                // pushing the error item so we don't leave a dangling
                // spinner alongside the error.
                self.finalize_in_flight();
                self.messages.push(MessageItem::Error { text: msg });
                self.phase = AgentPhase::Idle;
                self.set_mode(AppMode::Input);
                // Reset the per-turn finalization flag so the next turn
                // starts clean.
                self.turn_text_finalized = false;
            }
            StreamEvent::Status { message } => {
                // Live retry countdown: consecutive "Retrying ..." status
                // messages replace-in-place so the user sees a single line
                // that ticks down rather than N stale lines stacking up.
                if message.starts_with("Retrying ")
                    && let Some(MessageItem::Status { text }) = self.messages.last_mut()
                    && text.starts_with("Retrying ")
                {
                    *text = message;
                } else {
                    self.messages.push(MessageItem::Status { text: message });
                }
            }
            _ => {}
        }
    }

    /// Spinner character for current frame.
    pub fn spinner(&self) -> &str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[self.spinner_frame % FRAMES.len()]
    }
}

/// Actions the event loop should perform after handling an event.
#[derive(Debug)]
pub enum Action {
    SendChat(String),
    CancelChat,
    ListModels,
    SetModel(String),
    GetStatus,
    GetSubscriptionUsage,
    SetCwd(String),
    /// Queue a message on the server immediately (for Alt+Enter during streaming).
    /// The server will process it after the current agent turn finishes.
    QueueMessage(String),
    /// Inject a steering message into the running agent loop.
    Steer(String),
    /// Open the session picker overlay.
    OpenSessionPicker,
    /// Delete a session.
    DeleteSession(String),
    /// Set the tagline for a session.
    SetTagline {
        session_id: String,
        tagline: String,
    },
    /// Archive a session, optionally switching to another session first.
    ArchiveSession {
        session_id: String,
        switch_to: Option<String>,
    },
    /// Restore an archived session.
    RestoreSession {
        session_id: String,
    },
    /// Switch to viewing a different session.
    SwitchSession(String),
    /// Navigate back to previous session in nav stack.
    NavigateBack,
    /// List child sessions of the current session.
    ListChildren,
    /// Reload plugins for the current session.
    ReloadPlugins,
    /// Fork the current session: create a new session inheriting model/cwd/system_prompt.
    ForkSession,
    /// Create a fresh session with default settings.
    NewSession,
    /// Fire a hook on the server (best-effort, e.g. after TUI task state changes).
    FireHook {
        name: String,
        data: serde_json::Value,
    },
    /// Task-related actions that go through the protocol.
    TaskList {
        project: String,
        state: Option<String>,
    },
    TaskGet {
        id: i64,
    },
    TaskCreate {
        project: String,
        title: String,
    },
    TaskSearch {
        project: String,
        query: String,
    },
    TaskUpdate {
        id: i64,
        state: String,
    },
    TaskAssign {
        id: i64,
        session_id: String,
    },
    TaskStatus {
        project: String,
    },
    TaskMergeQueue {
        project: String,
    },
    /// Open the task picker overlay.
    OpenTaskPicker,
    /// Open the task picker overlay pre-filtered to tasks in the given
    /// state.  Used by `/task active` and similar quick-switch commands.
    OpenTaskPickerWithState {
        state: String,
    },
    /// Dispatch a task (schedule if needed + create session).
    TaskDispatch {
        id: i64,
    },
    /// Run scheduling pass.
    TaskSchedule {
        project: String,
    },
    /// Merge approved task.
    TaskMerge {
        id: i64,
    },
}

/// Convert a crossterm KeyEvent to a tui_textarea compatible input event.
fn event_to_tui_textarea(key: &KeyEvent) -> crossterm::event::Event {
    crossterm::event::Event::Key(*key)
}

/// Current unix timestamp in seconds, clamped to 0 on clock skew.
pub(crate) fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format a "time since then" delta as a short human string.
///
/// `now_secs` and `then_secs` are both unix seconds.  Returns
/// `"now"` / `"Nm ago"` / `"Nh ago"` / `"Nd ago"`; returns
/// `"in the future"` if `then` is after `now`, or `"?"` if `then <= 0`.
pub(crate) fn format_age_since(now_secs: i64, then_secs: i64) -> String {
    if then_secs <= 0 {
        return "?".into();
    }
    let delta = now_secs - then_secs;
    if delta < 0 {
        return "in the future".into();
    }
    if delta < 60 {
        return "now".into();
    }
    let m = delta / 60;
    if m < 60 {
        return format!("{}m ago", m);
    }
    let h = delta / 3600;
    if h < 24 {
        return format!("{}h ago", h);
    }
    let d = delta / 86400;
    format!("{}d ago", d)
}

/// Same as [`format_age_since`] but both inputs are unix milliseconds.
pub(crate) fn format_age_since_ms(now_ms: i64, then_ms: i64) -> String {
    format_age_since(now_ms / 1000, then_ms / 1000)
}

/// Render one `task_history` entry as a compact human-readable string.
///
/// State transitions are formatted as `state  old → new`; other fields use
/// `field: old → new` (falling back to `field: new` when `old` is unknown).
pub(crate) fn render_history_entry(entry: &TaskHistoryInfo) -> String {
    let old = entry.old_value.as_deref().unwrap_or("");
    let new = entry.new_value.as_deref().unwrap_or("");
    match entry.field.as_str() {
        "state" => format!("state    {} \u{2192} {}", old, new),
        other => {
            if old.is_empty() {
                format!("{}: {}", other, new)
            } else {
                format!("{}: {} \u{2192} {}", other, old, new)
            }
        }
    }
}

/// Preference order for resolving a task's "primary" session: worker beats
/// reviewer beats refiner beats planner beats interactive beats creator.
/// Unknown roles sort last.
fn role_priority(role: &str) -> u8 {
    match role {
        "worker" => 0,
        "reviewer" => 1,
        "refiner" => 2,
        "planner" => 3,
        "interactive" => 4,
        "creator" => 5,
        _ => 99,
    }
}

/// Pick the most useful session to jump to for a task.
///
/// Preference:
///   1. any live non-archived session (prefer higher role_priority, then
///      most recent activity);
///   2. any non-archived session;
///   3. `task.session_id` (the task's canonical assigned session).
pub(crate) fn primary_session_id(task: &TaskInfo, sessions: &[TaskSessionInfo]) -> Option<String> {
    // Candidates that are non-archived.
    let mut live: Vec<&TaskSessionInfo> = sessions
        .iter()
        .filter(|s| s.is_live && s.archived != Some(true))
        .collect();
    live.sort_by(|a, b| {
        role_priority(&a.role)
            .cmp(&role_priority(&b.role))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    if let Some(s) = live.first() {
        return Some(s.session_id.clone());
    }

    let mut ok: Vec<&TaskSessionInfo> = sessions
        .iter()
        .filter(|s| s.archived != Some(true))
        .collect();
    ok.sort_by(|a, b| {
        role_priority(&a.role)
            .cmp(&role_priority(&b.role))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    if let Some(s) = ok.first() {
        return Some(s.session_id.clone());
    }

    task.session_id.clone()
}

/// Sort sessions into tree order: roots first (by last_activity desc),
/// each followed by its children recursively (also by last_activity desc).
fn tree_sort_sessions(sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
    use std::collections::HashMap;

    // Build parent -> children index map
    let mut children_of: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        children_of
            .entry(s.parent_id.as_deref())
            .or_default()
            .push(i);
    }

    // Sort children within each group by last_activity descending
    for group in children_of.values_mut() {
        group.sort_by(|&a, &b| sessions[b].last_activity.cmp(&sessions[a].last_activity));
    }

    // DFS walk to build ordered index list
    let mut order: Vec<usize> = Vec::with_capacity(sessions.len());
    fn walk(
        parent: Option<&str>,
        sessions: &[SessionInfo],
        children_of: &HashMap<Option<&str>, Vec<usize>>,
        order: &mut Vec<usize>,
    ) {
        if let Some(children) = children_of.get(&parent) {
            for &idx in children {
                order.push(idx);
                walk(Some(&sessions[idx].id), sessions, children_of, order);
            }
        }
    }
    walk(None, &sessions, &children_of, &mut order);

    // Add any orphans (parent_id set but parent not in list)
    let in_tree: std::collections::HashSet<usize> = order.iter().copied().collect();
    for i in 0..sessions.len() {
        if !in_tree.contains(&i) {
            order.push(i);
        }
    }

    // Reorder sessions by extracting in order (swap-based to avoid clone)
    // Use a simpler approach: collect into a new Vec
    let mut result = Vec::with_capacity(sessions.len());
    // Mark slots as taken
    let mut taken = vec![false; sessions.len()];
    for &idx in &order {
        taken[idx] = true;
    }
    // We need to move out of sessions by index. Use Option wrapping.
    let mut slots: Vec<Option<SessionInfo>> = sessions.into_iter().map(Some).collect();
    for &idx in &order {
        if let Some(s) = slots[idx].take() {
            result.push(s);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau_agent::types::{AssistantMessage, TextContent, Usage};

    fn make_app() -> App {
        App::new(
            "sess-test".to_string(),
            "test-model".to_string(),
            "test-provider".to_string(),
            crate::theme::dark(),
        )
    }

    fn assistant_message(
        text: &str,
        stop_reason: StopReason,
        error_message: Option<&str>,
    ) -> AssistantMessage {
        let content = if text.is_empty() {
            Vec::new()
        } else {
            vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })]
        };
        AssistantMessage {
            content,
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason,
            error_message: error_message.map(String::from),
            timestamp: 0,
        }
    }

    /// Push a streaming placeholder followed by a `Done` whose message has
    /// no content and `stop_reason: Error`.  The placeholder must be
    /// replaced by an `Error` item (not an empty `Assistant`), and the
    /// app must return to `Input` mode without waiting for `Phase::Idle`.
    #[test]
    fn done_with_empty_error_replaces_placeholder_with_error() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.messages.push(MessageItem::AssistantStreaming {
            text: String::new(),
        });

        let event = StreamEvent::Done {
            reason: StopReason::Error,
            message: assistant_message("", StopReason::Error, Some("boom")),
        };
        app.handle_stream_event(event);

        assert_eq!(app.mode, AppMode::Input, "mode should reset to Input");
        assert_eq!(app.phase, AgentPhase::Idle);
        // No dangling streaming placeholder.
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::AssistantStreaming { .. })),
            "streaming placeholder should be gone"
        );
        // Last message is an Error carrying the error text.
        match app.messages.last() {
            Some(MessageItem::Error { text }) => assert_eq!(text, "boom"),
            other => panic!("expected MessageItem::Error, got {other:?}"),
        }
    }

    /// `Done` with empty content and `StopReason::Error` but no
    /// `error_message` should still surface a non-empty error item.
    #[test]
    fn done_with_empty_error_uses_fallback_text_when_no_error_message() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.messages.push(MessageItem::AssistantStreaming {
            text: String::new(),
        });

        let event = StreamEvent::Done {
            reason: StopReason::Error,
            message: assistant_message("", StopReason::Error, None),
        };
        app.handle_stream_event(event);

        match app.messages.last() {
            Some(MessageItem::Error { text }) => {
                assert!(!text.is_empty(), "fallback error text should be non-empty")
            }
            other => panic!("expected MessageItem::Error, got {other:?}"),
        }
        assert_eq!(app.mode, AppMode::Input);
    }

    /// `Done` with normal text content should still finalize the
    /// placeholder to `Assistant` *and* clear streaming mode (no longer
    /// dependent on a separate `Phase::Idle` event).
    #[test]
    fn done_with_text_finalizes_placeholder_and_clears_mode() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.messages.push(MessageItem::AssistantStreaming {
            text: "hello".to_string(),
        });

        let event = StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("hello world", StopReason::Stop, None),
        };
        app.handle_stream_event(event);

        assert_eq!(app.mode, AppMode::Input);
        assert_eq!(app.phase, AgentPhase::Idle);
        match app.messages.last() {
            Some(MessageItem::Assistant { text }) => assert_eq!(text, "hello world"),
            other => panic!("expected MessageItem::Assistant, got {other:?}"),
        }
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::AssistantStreaming { .. })),
            "streaming placeholder should have been replaced"
        );
    }

    /// `Done` arriving with no prior streaming placeholder (e.g. server
    /// sent the final message in a single shot) should still surface the
    /// text as a fresh assistant message instead of dropping it.
    #[test]
    fn done_without_placeholder_appends_assistant_message() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;

        let event = StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("solo response", StopReason::Stop, None),
        };
        app.handle_stream_event(event);

        assert_eq!(app.mode, AppMode::Input);
        match app.messages.last() {
            Some(MessageItem::Assistant { text }) => assert_eq!(text, "solo response"),
            other => panic!("expected MessageItem::Assistant, got {other:?}"),
        }
    }

    /// `StreamEvent::Error` arriving while a streaming placeholder is
    /// still in the message list must finalize that placeholder before
    /// pushing the new error item — we should never end up with
    /// `[..., AssistantStreaming, Error]` adjacent.
    #[test]
    fn error_finalizes_dangling_streaming_placeholder() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        // Non-empty streaming text: should be converted to a final
        // Assistant entry rather than dropped.
        app.messages.push(MessageItem::AssistantStreaming {
            text: "partial".to_string(),
        });

        let event = StreamEvent::Error {
            reason: StopReason::Error,
            error: assistant_message("", StopReason::Error, Some("network down")),
        };
        app.handle_stream_event(event);

        assert_eq!(app.mode, AppMode::Input);
        assert_eq!(app.phase, AgentPhase::Idle);
        // No dangling streaming items left.
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::AssistantStreaming { .. })),
            "streaming placeholder must be finalized before the error item"
        );
        // Expect [..., Assistant("partial"), Error("network down")]
        assert!(app.messages.len() >= 2);
        match &app.messages[app.messages.len() - 2] {
            MessageItem::Assistant { text } => assert_eq!(text, "partial"),
            other => panic!("expected finalized Assistant, got {other:?}"),
        }
        match app.messages.last() {
            Some(MessageItem::Error { text }) => assert_eq!(text, "network down"),
            other => panic!("expected MessageItem::Error, got {other:?}"),
        }
    }

    /// An `Error` event whose streaming placeholder was empty should
    /// drop the placeholder entirely (not leave an empty Assistant
    /// alongside the error).
    #[test]
    fn error_drops_empty_streaming_placeholder() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.messages.push(MessageItem::AssistantStreaming {
            text: String::new(),
        });

        let event = StreamEvent::Error {
            reason: StopReason::Error,
            error: assistant_message("", StopReason::Error, Some("nope")),
        };
        app.handle_stream_event(event);

        // Only the Error item should remain.
        assert_eq!(app.messages.len(), 1);
        assert!(matches!(
            app.messages.last(),
            Some(MessageItem::Error { .. })
        ));
        assert_eq!(app.mode, AppMode::Input);
    }

    /// Regression test for task #425: a normal text-only turn
    /// (Start → TextStart → TextDelta → TextEnd → Done) must result in
    /// *exactly one* final `Assistant` message, not two.  Before the
    /// fix, `TextEnd` converted the streaming placeholder to `Assistant`
    /// and then `Done`'s fallback branch appended a duplicate copy.
    #[test]
    fn normal_text_turn_renders_once_not_twice() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;

        // 1. Start: push streaming placeholder.
        app.handle_stream_event(StreamEvent::Start {
            partial: assistant_message("", StopReason::Stop, None),
        });
        // 2. TextStart: noop (placeholder already present).
        app.handle_stream_event(StreamEvent::TextStart {
            content_index: 0,
            partial: assistant_message("", StopReason::Stop, None),
        });
        // 3. TextDelta × 2: append to placeholder.
        app.handle_stream_event(StreamEvent::TextDelta {
            content_index: 0,
            delta: "hello ".to_string(),
            partial: assistant_message("hello ", StopReason::Stop, None),
        });
        app.handle_stream_event(StreamEvent::TextDelta {
            content_index: 0,
            delta: "world".to_string(),
            partial: assistant_message("hello world", StopReason::Stop, None),
        });
        // 4. TextEnd: convert placeholder to final Assistant.
        app.handle_stream_event(StreamEvent::TextEnd {
            content_index: 0,
            content: "hello world".to_string(),
            partial: assistant_message("hello world", StopReason::Stop, None),
        });
        // 5. Done: final message carries the same text.
        app.handle_stream_event(StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("hello world", StopReason::Stop, None),
        });

        // Exactly one Assistant message, no duplicates, no dangling
        // streaming placeholder.
        let assistant_count = app
            .messages
            .iter()
            .filter(|m| matches!(m, MessageItem::Assistant { .. }))
            .count();
        assert_eq!(
            assistant_count, 1,
            "expected exactly one Assistant message, got {assistant_count}: {:?}",
            app.messages
        );
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::AssistantStreaming { .. })),
            "streaming placeholder should have been finalized"
        );
        match app
            .messages
            .iter()
            .find(|m| matches!(m, MessageItem::Assistant { .. }))
        {
            Some(MessageItem::Assistant { text }) => assert_eq!(text, "hello world"),
            other => panic!("expected Assistant, got {other:?}"),
        }
        // `Done` clears the per-turn finalization flag so the next turn
        // starts clean.
        assert!(!app.turn_text_finalized);
        assert_eq!(app.mode, AppMode::Input);
        assert_eq!(app.phase, AgentPhase::Idle);
    }

    /// Safety-net path from task #421: `Done` arrives with text content
    /// but no prior `Start`/`TextStart`/`TextDelta`/`TextEnd` events
    /// (e.g. a server shortcut path) — the message must still be
    /// rendered exactly once.
    #[test]
    fn done_without_any_text_events_appends_exactly_once() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        // No prior events: `turn_text_finalized` stays false.

        app.handle_stream_event(StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("solo response", StopReason::Stop, None),
        });

        let assistant_count = app
            .messages
            .iter()
            .filter(|m| matches!(m, MessageItem::Assistant { .. }))
            .count();
        assert_eq!(assistant_count, 1);
        match app
            .messages
            .iter()
            .find(|m| matches!(m, MessageItem::Assistant { .. }))
        {
            Some(MessageItem::Assistant { text }) => assert_eq!(text, "solo response"),
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert_eq!(app.mode, AppMode::Input);
    }

    /// Task #421's original concern: `Done` with empty content and
    /// `StopReason::Error` yields a *single* `Error` item (and no stray
    /// duplicates via the text fallback).
    #[test]
    fn done_with_empty_error_produces_exactly_one_error() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.messages.push(MessageItem::AssistantStreaming {
            text: String::new(),
        });

        app.handle_stream_event(StreamEvent::Done {
            reason: StopReason::Error,
            message: assistant_message("", StopReason::Error, Some("boom")),
        });

        let error_count = app
            .messages
            .iter()
            .filter(|m| matches!(m, MessageItem::Error { .. }))
            .count();
        assert_eq!(error_count, 1);
        let assistant_count = app
            .messages
            .iter()
            .filter(|m| matches!(m, MessageItem::Assistant { .. }))
            .count();
        assert_eq!(assistant_count, 0);
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::AssistantStreaming { .. })),
            "no dangling streaming placeholder"
        );
    }

    /// Simulate two consecutive text-only turns in a row.  The flag
    /// must be reset by the first `Done` so the second turn behaves
    /// identically and produces exactly one message (not two and not
    /// zero).
    #[test]
    fn two_consecutive_text_turns_each_render_once() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;

        for text in ["first reply", "second reply"] {
            app.handle_stream_event(StreamEvent::Start {
                partial: assistant_message("", StopReason::Stop, None),
            });
            app.handle_stream_event(StreamEvent::TextStart {
                content_index: 0,
                partial: assistant_message("", StopReason::Stop, None),
            });
            app.handle_stream_event(StreamEvent::TextDelta {
                content_index: 0,
                delta: text.to_string(),
                partial: assistant_message(text, StopReason::Stop, None),
            });
            app.handle_stream_event(StreamEvent::TextEnd {
                content_index: 0,
                content: text.to_string(),
                partial: assistant_message(text, StopReason::Stop, None),
            });
            app.handle_stream_event(StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_message(text, StopReason::Stop, None),
            });
        }

        let assistant_texts: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|m| match m {
                MessageItem::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_texts,
            vec!["first reply", "second reply"],
            "each turn should produce exactly one Assistant message in order"
        );
    }

    // ---- Session picker stability tests ----

    /// Helper: put the app into SessionPicker mode with a given previous mode.
    fn open_picker(app: &mut App, previous: AppMode) {
        app.mode = previous;
        app.picker_previous_mode = previous;
        app.mode = AppMode::SessionPicker;
    }

    /// AgentDone while picker is open should NOT close the picker.
    #[test]
    fn picker_stays_open_on_agent_done() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::AgentDone);

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Stream events arriving while picker is open (previous mode = Input)
    /// should update picker_previous_mode to Streaming, not close the picker.
    #[test]
    fn picker_stays_open_on_stream_text_delta() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Input);

        app.handle_server_response(Response::Stream {
            event: Box::new(StreamEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_string(),
                partial: assistant_message("hello", StopReason::Stop, None),
            }),
        });

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Streaming,
            "underlying mode should switch to Streaming"
        );
    }

    /// Phase::Idle while picker is open and underlying mode is Streaming
    /// should update picker_previous_mode to Input, not close the picker.
    #[test]
    fn picker_stays_open_on_phase_idle() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::Stream {
            event: Box::new(StreamEvent::Phase {
                phase: AgentPhase::Idle,
            }),
        });

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Cancelled response while picker is open should NOT close the picker.
    #[test]
    fn picker_stays_open_on_cancelled() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::Cancelled);

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Error response while picker is open should NOT close the picker.
    #[test]
    fn picker_stays_open_on_error() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Input);

        app.handle_server_response(Response::Error {
            message: "something broke".to_string(),
        });

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should stay Input"
        );
    }

    /// StreamEvent::Done while picker is open should NOT close the picker.
    #[test]
    fn picker_stays_open_on_stream_done() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Streaming);

        app.handle_stream_event(StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("done", StopReason::Stop, None),
        });

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// StreamEvent::Error while picker is open should NOT close the picker.
    #[test]
    fn picker_stays_open_on_stream_error() {
        let mut app = make_app();
        open_picker(&mut app, AppMode::Streaming);

        app.handle_stream_event(StreamEvent::Error {
            reason: StopReason::Error,
            error: assistant_message("err", StopReason::Error, Some("fail")),
        });

        assert_eq!(app.mode, AppMode::SessionPicker, "picker must stay open");
        assert_eq!(
            app.picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    // ---- Task picker tests ----

    /// Helper: create a test TaskInfo.
    fn make_task_info(id: i64, title: &str, state: &str) -> TaskInfo {
        TaskInfo {
            id,
            project_name: "test-project".into(),
            title: title.into(),
            state: state.into(),
            priority: 0,
            parent_id: None,
            tags: Some(serde_json::json!(["tui", "test"])),
            affected_files: None,
            branch: None,
            worktree_path: None,
            session_id: Some("s100".into()),
            skip_review: false,
            require_approval: false,
            sandbox_profile: None,
            held: false,
            has_live_session: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Helper: put the app into TaskPicker mode with a given previous mode.
    fn open_task_picker(app: &mut App, previous: AppMode) {
        app.mode = previous;
        app.task_picker_previous_mode = previous;
        app.mode = AppMode::TaskPicker;
    }

    /// Helper: populate picker_tasks with test data.
    fn populate_picker_tasks(app: &mut App) {
        app.picker_tasks = vec![
            (0, make_task_info(1, "First task", "active")),
            (1, make_task_info(2, "Second task", "review")),
            (0, make_task_info(3, "Third task", "approved")),
        ];
    }

    /// AgentDone while task picker is open should NOT close the picker.
    #[test]
    fn task_picker_stays_open_on_agent_done() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::AgentDone);

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Stream events arriving while task picker is open should NOT close it.
    #[test]
    fn task_picker_stays_open_on_stream_text_delta() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);

        app.handle_server_response(Response::Stream {
            event: Box::new(StreamEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_string(),
                partial: assistant_message("hello", StopReason::Stop, None),
            }),
        });

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Streaming,
            "underlying mode should switch to Streaming"
        );
    }

    /// Phase::Idle while task picker is open should NOT close it.
    #[test]
    fn task_picker_stays_open_on_phase_idle() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::Stream {
            event: Box::new(StreamEvent::Phase {
                phase: AgentPhase::Idle,
            }),
        });

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Cancelled response while task picker is open should NOT close it.
    #[test]
    fn task_picker_stays_open_on_cancelled() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Streaming);

        app.handle_server_response(Response::Cancelled);

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// Error response while task picker is open should NOT close it.
    #[test]
    fn task_picker_stays_open_on_error() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);

        app.handle_server_response(Response::Error {
            message: "something broke".to_string(),
        });

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Input,
            "underlying mode should stay Input"
        );
    }

    /// StreamEvent::Done while task picker is open should NOT close it.
    #[test]
    fn task_picker_stays_open_on_stream_done() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Streaming);

        app.handle_stream_event(StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message("done", StopReason::Stop, None),
        });

        assert_eq!(app.mode, AppMode::TaskPicker, "task picker must stay open");
        assert_eq!(
            app.task_picker_previous_mode,
            AppMode::Input,
            "underlying mode should transition to Input"
        );
    }

    /// F2 closes task picker and restores previous mode.
    #[test]
    fn task_picker_close_restores_mode() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        let key = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        app.handle_task_picker_key(&key);

        assert_eq!(app.mode, AppMode::Input, "should restore previous mode");
    }

    /// Esc closes task picker and restores previous mode.
    #[test]
    fn task_picker_esc_closes() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Streaming);
        populate_picker_tasks(&mut app);

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_task_picker_key(&key);

        assert_eq!(app.mode, AppMode::Streaming, "should restore previous mode");
    }

    /// Filter matches title, id, state, and tags.
    #[test]
    fn task_picker_filter_matches() {
        let mut app = make_app();
        populate_picker_tasks(&mut app);

        // Match by title
        app.task_picker_filter = "First".into();
        let indices = app.task_picker_filtered_indices();
        assert_eq!(indices, vec![0]);

        // Match by ID prefix
        app.task_picker_filter = "2".into();
        let indices = app.task_picker_filtered_indices();
        assert_eq!(indices, vec![1]);

        // Match by state
        app.task_picker_filter = "approved".into();
        let indices = app.task_picker_filtered_indices();
        assert_eq!(indices, vec![2]);

        // Match by tag
        app.task_picker_filter = "tui".into();
        let indices = app.task_picker_filtered_indices();
        assert_eq!(indices, vec![0, 1, 2]);

        // Empty filter matches all
        app.task_picker_filter = String::new();
        let indices = app.task_picker_filtered_indices();
        assert_eq!(indices, vec![0, 1, 2]);

        // No match
        app.task_picker_filter = "nonexistent".into();
        let indices = app.task_picker_filtered_indices();
        assert!(indices.is_empty());
    }

    /// Confirm approve flow: 'a' sets confirm, 'y' executes.
    #[test]
    fn task_picker_confirm_approve_flow() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        // Press 'a' to set confirmation
        let key_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_a);
        assert!(action.is_none());
        assert!(app.task_picker_confirm.is_some());

        // Press 'y' to confirm
        let key_y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_y);
        assert!(action.is_some());
        match action {
            Some(Action::TaskUpdate { id, state }) => {
                assert_eq!(id, 1); // first task
                assert_eq!(state, "approved");
            }
            other => panic!("expected TaskUpdate, got {other:?}"),
        }
        assert!(app.task_picker_confirm.is_none());
    }

    /// Confirm cancel: any non-y/Enter key cancels the confirmation.
    #[test]
    fn task_picker_confirm_cancel() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        // Press 'a' to set confirmation
        let key_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_task_picker_key(&key_a);
        assert!(app.task_picker_confirm.is_some());

        // Press 'n' to cancel
        let key_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_n);
        assert!(action.is_none());
        assert!(app.task_picker_confirm.is_none());
    }

    /// Create mode flow: 'c' enters filter+create mode, Enter creates task.
    #[test]
    fn task_picker_create_mode_flow() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        app.session_cwd = Some("/test-project".into());
        app.session_project_name = Some("test-project".into());
        let key_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        app.handle_task_picker_key(&key_c);
        assert!(app.task_picker_filter_mode);
        assert!(app.task_picker_create_mode);

        // Type "New task"
        for ch in "New task".chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            app.handle_task_picker_key(&key);
        }
        assert_eq!(app.task_picker_filter, "New task");

        // Press Enter to create
        let key_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_enter);
        assert!(!app.task_picker_filter_mode);
        assert!(!app.task_picker_create_mode);
        match action {
            Some(Action::TaskCreate { project, title }) => {
                assert_eq!(project, "test-project");
                assert_eq!(title, "New task");
            }
            other => panic!("expected TaskCreate, got {other:?}"),
        }
    }

    /// Detail view opens (via response) and Esc closes it.
    #[test]
    fn task_picker_detail_open_close() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        // Simulate detail response
        app.task_picker_detail = Some(Box::new(TaskPickerDetail {
            task: make_task_info(1, "First task", "active"),
            messages: vec![],
            relations: vec![],
            subtasks: vec![],
            sessions: vec![],
            history: vec![],
            scroll: 0,
        }));
        assert!(app.task_picker_detail.is_some());

        // Press Esc to close detail
        let key_esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_task_picker_key(&key_esc);
        assert!(app.task_picker_detail.is_none());
    }

    /// Enter on a task returns TaskGet action to fetch detail.
    #[test]
    fn task_picker_enter_fetches_detail() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        let key_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_enter);
        match action {
            Some(Action::TaskGet { id }) => assert_eq!(id, 1),
            other => panic!("expected TaskGet, got {other:?}"),
        }
    }

    /// 'g' from normal mode goes to session.
    #[test]
    fn task_picker_go_to_session() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        populate_picker_tasks(&mut app);

        let key_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        let action = app.handle_task_picker_key(&key_g);
        match action {
            Some(Action::SwitchSession(sid)) => assert_eq!(sid, "s100"),
            other => panic!("expected SwitchSession, got {other:?}"),
        }
        // Picker should be closed
        assert_eq!(app.mode, AppMode::Input);
    }

    /// TaskTree response in TaskPicker mode populates picker_tasks.
    #[test]
    fn task_picker_response_task_tree() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);

        let tasks = vec![
            (0, make_task_info(10, "Root task", "active")),
            (1, make_task_info(11, "Child task", "planning")),
        ];
        let action = app.handle_server_response(Response::TaskTree { tasks });
        assert!(action.is_none());
        assert_eq!(app.picker_tasks.len(), 2);
        assert_eq!(app.task_picker_cursor, 0);
    }

    /// TaskDetail response in TaskPicker mode populates task_picker_detail.
    #[test]
    fn task_picker_response_task_detail() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);

        let action = app.handle_server_response(Response::TaskDetail {
            task: make_task_info(10, "Test task", "active"),
            messages: vec![],
            relations: vec![],
            subtasks: vec![],
            sessions: vec![],
            history: vec![],
        });
        assert!(action.is_none());
        assert!(app.task_picker_detail.is_some());
        assert_eq!(
            app.task_picker_detail
                .as_ref()
                .expect("should be Some")
                .task
                .id,
            10
        );
    }

    /// TaskUpdated response in TaskPicker mode triggers a refresh.
    #[test]
    fn task_picker_response_task_updated_refreshes() {
        let mut app = make_app();
        open_task_picker(&mut app, AppMode::Input);
        app.session_cwd = Some("/test-project".into());
        app.session_project_name = Some("test-project".into());

        let action = app.handle_server_response(Response::TaskUpdated {
            task: make_task_info(10, "Test task", "approved"),
        });
        // Should return a TaskList action to refresh
        match action {
            Some(Action::TaskList { project, state }) => {
                assert_eq!(project, "test-project");
            }
            other => panic!("expected TaskList for refresh, got {other:?}"),
        }
    }

    // ---- Steer indicator tests ----

    /// Sending a steer message sets `pending_steer`.
    #[test]
    fn steer_sets_pending_steer() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("fix the bug");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let action = app.handle_streaming_key(&key);

        assert!(matches!(action, Some(Action::Steer(ref t)) if t == "fix the bug"));
        assert_eq!(app.pending_steer.as_deref(), Some("fix the bug"));
    }

    /// StreamEvent::Start clears pending_steer (session acknowledged the steer).
    #[test]
    fn stream_start_clears_pending_steer() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.pending_steer = Some("steer text".into());

        app.handle_stream_event(StreamEvent::Start {
            partial: assistant_message("", StopReason::Stop, None),
        });

        assert!(
            app.pending_steer.is_none(),
            "Start should clear pending_steer"
        );
    }

    /// AgentDone clears pending_steer.
    #[test]
    fn agent_done_clears_pending_steer() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.pending_steer = Some("steer text".into());

        app.handle_server_response(Response::AgentDone);

        assert!(
            app.pending_steer.is_none(),
            "AgentDone should clear pending_steer"
        );
    }

    /// pending_steer starts as None.
    #[test]
    fn pending_steer_starts_none() {
        let app = make_app();
        assert!(app.pending_steer.is_none());
    }

    // ---- Alt+Enter (queue message) tests ----

    /// Regression: Alt+Enter while the session is idle (Input mode) should send
    /// the message immediately as a regular chat (Action::SendChat), not buffer
    /// it in a client-side queue that is only drained on the next server event.
    #[test]
    fn alt_enter_in_input_mode_sends_immediately() {
        let mut app = make_app();
        app.mode = AppMode::Input;
        app.textarea.insert_str("hello from idle");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        let action = app.handle_input_key(&key);

        // Should produce a SendChat action, not None
        assert!(
            matches!(action, Some(Action::SendChat(ref t)) if t == "hello from idle"),
            "Alt+Enter in Input mode should send immediately, got {action:?}"
        );
        // Mode should transition to Streaming
        assert_eq!(app.mode, AppMode::Streaming);
        // Textarea should be cleared
        assert!(app.textarea.lines().iter().all(|l: &String| l.is_empty()));
    }

    /// Alt+Enter while streaming should send a QueueMessage to the server
    /// immediately — no client-side buffering.
    #[test]
    fn alt_enter_in_streaming_mode_sends_queue_message() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("run after current turn");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        let action = app.handle_streaming_key(&key);

        // Should produce a QueueMessage action
        assert!(
            matches!(action, Some(Action::QueueMessage(ref t)) if t == "run after current turn"),
            "Alt+Enter in Streaming mode should return QueueMessage, got {action:?}"
        );
        // Mode should remain Streaming (we're still busy)
        assert_eq!(app.mode, AppMode::Streaming);
        // Textarea should be cleared
        assert!(app.textarea.lines().iter().all(|l: &String| l.is_empty()));
        // A "[queued: ...]" status message should be displayed
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, MessageItem::Status { text } if text.contains("queued"))),
            "should show queued status message"
        );
    }

    // ---- Input history (slash command scrollback) tests ----

    /// Regular chat messages are added to input_history.
    #[test]
    fn input_history_includes_regular_messages() {
        let mut app = make_app();
        app.mode = AppMode::Input;
        app.textarea.insert_str("hello world");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["hello world"]);
    }

    /// Slash commands are added to input_history.
    #[test]
    fn input_history_includes_slash_commands() {
        let mut app = make_app();
        app.mode = AppMode::Input;
        app.textarea.insert_str("/help");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["/help"]);
    }

    /// Both regular messages and slash commands appear in input_history
    /// in order.
    #[test]
    fn input_history_interleaves_chat_and_slash() {
        let mut app = make_app();
        app.mode = AppMode::Input;

        // Send a regular message
        app.textarea.insert_str("first");
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_input_key(&key);

        // Reset mode (SendChat switches to Streaming)
        app.mode = AppMode::Input;

        // Send a slash command
        app.textarea.insert_str("/help");
        app.handle_input_key(&key);

        // Send another regular message
        app.mode = AppMode::Input;
        app.textarea.insert_str("second");
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["first", "/help", "second"]);
    }

    /// Consecutive duplicate entries are deduplicated.
    #[test]
    fn input_history_deduplicates_consecutive() {
        let mut app = make_app();
        app.mode = AppMode::Input;

        app.textarea.insert_str("/help");
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_input_key(&key);

        app.textarea.insert_str("/help");
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["/help"]);
    }

    /// Non-consecutive duplicate entries are NOT deduplicated.
    #[test]
    fn input_history_keeps_nonconsecutive_duplicates() {
        let mut app = make_app();
        app.mode = AppMode::Input;

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        app.textarea.insert_str("/help");
        app.handle_input_key(&key);

        app.mode = AppMode::Input;
        app.textarea.insert_str("/status");
        app.handle_input_key(&key);

        app.mode = AppMode::Input;
        app.textarea.insert_str("/help");
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["/help", "/status", "/help"]);
    }

    /// Up arrow retrieves the last slash command from history.
    #[test]
    fn up_arrow_retrieves_slash_command() {
        let mut app = make_app();
        app.mode = AppMode::Input;

        // Add a slash command to history
        app.textarea.insert_str("/model");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_input_key(&enter);

        // Press up arrow
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_input_key(&up);

        assert_eq!(app.textarea.lines().join("\n"), "/model");
        assert_eq!(app.history_index, Some(0));
    }

    /// Slash commands sent during streaming (steer) are added to history.
    #[test]
    fn slash_command_during_streaming_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("/help");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_streaming_key(&key);

        assert_eq!(app.input_history, vec!["/help"]);
    }

    /// Steer messages during streaming are added to history.
    #[test]
    fn steer_message_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("fix the bug");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_streaming_key(&key);

        assert_eq!(app.input_history, vec!["fix the bug"]);
    }

    /// Alt+Enter (queue message) during streaming adds to history.
    #[test]
    fn queued_message_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("queued text");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        app.handle_streaming_key(&key);

        assert_eq!(app.input_history, vec!["queued text"]);
    }

    /// Alt+Enter with slash command during streaming adds to history.
    #[test]
    fn alt_enter_slash_during_streaming_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;
        app.textarea.insert_str("/help");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        app.handle_streaming_key(&key);

        assert_eq!(app.input_history, vec!["/help"]);
    }

    /// Alt+Enter in input mode adds to history.
    #[test]
    fn alt_enter_in_input_mode_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Input;
        app.textarea.insert_str("alt msg");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["alt msg"]);
    }

    /// Alt+Enter with slash command in input mode adds to history.
    #[test]
    fn alt_enter_slash_in_input_mode_added_to_history() {
        let mut app = make_app();
        app.mode = AppMode::Input;
        app.textarea.insert_str("/help");

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        app.handle_input_key(&key);

        assert_eq!(app.input_history, vec!["/help"]);
    }

    /// Restored user messages populate input_history for scrollback.
    #[test]
    fn restore_messages_populates_input_history() {
        let mut app = make_app();

        use tau_agent::types::{TextContent, UserContent, UserMessage};
        let messages = vec![
            Message::User(UserMessage {
                content: vec![UserContent::Text(TextContent {
                    text: "first message".into(),
                    text_signature: None,
                })],
                timestamp: 0,
            }),
            Message::User(UserMessage {
                content: vec![UserContent::Text(TextContent {
                    text: "second message".into(),
                    text_signature: None,
                })],
                timestamp: 0,
            }),
        ];

        app.restore_messages(&messages);

        assert_eq!(app.input_history, vec!["first message", "second message"]);
    }

    // ---- retry countdown replace-in-place tests ----

    /// A series of "Retrying ..." status events must collapse into a single
    /// message that gets overwritten in place, so the user sees one live
    /// countdown line rather than N stacked stale lines.
    #[test]
    fn retry_status_replaces_in_place() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;

        for secs in [5u64, 4, 3, 2, 1] {
            app.handle_stream_event(StreamEvent::Status {
                message: format!("Retrying (attempt 1/3) in {secs}s... (timeout: boom)"),
            });
        }

        let retry_statuses: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|m| match m {
                MessageItem::Status { text } if text.starts_with("Retrying ") => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            retry_statuses.len(),
            1,
            "5 consecutive retry status events should collapse to 1, got {retry_statuses:?}"
        );
        assert!(
            retry_statuses[0].contains("in 1s"),
            "final status should carry the last countdown value, got {:?}",
            retry_statuses[0]
        );
    }

    /// A non-retry status following a retry status must not overwrite the
    /// retry line — only consecutive retry messages collapse.
    #[test]
    fn non_retry_status_does_not_replace_retry() {
        let mut app = make_app();
        app.mode = AppMode::Streaming;

        app.handle_stream_event(StreamEvent::Status {
            message: "Retrying (attempt 1/3) in 3s... (timeout: boom)".into(),
        });
        app.handle_stream_event(StreamEvent::Status {
            message: "some other status".into(),
        });
        app.handle_stream_event(StreamEvent::Status {
            message: "Retrying (attempt 1/3) in 2s... (timeout: boom)".into(),
        });

        let statuses: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|m| match m {
                MessageItem::Status { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        // We expect three status items: retry(3s), other, retry(2s).
        assert_eq!(
            statuses.len(),
            3,
            "expected 3 distinct status items, got {statuses:?}"
        );
        assert!(statuses[0].contains("in 3s"));
        assert_eq!(statuses[1], "some other status");
        assert!(statuses[2].contains("in 2s"));
    }

    // ---- Enriched task detail tests ----

    fn make_task_session(role: &str, sid: &str) -> TaskSessionInfo {
        TaskSessionInfo {
            session_id: sid.into(),
            role: role.into(),
            created_at: 1_000_000,
            message_count: Some(42),
            archived: Some(false),
            last_activity: Some(now_secs() - 120),
            last_phase: Some("idle".into()),
            last_exit_status: Some("completed".into()),
            is_live: false,
        }
    }

    #[test]
    fn format_age_since_human_buckets() {
        assert_eq!(format_age_since(1000, 0), "?");
        assert_eq!(format_age_since(1000, 1010), "in the future");
        // Up to 59 seconds renders as "now".
        assert_eq!(format_age_since(1000, 990), "now");
        assert_eq!(format_age_since(1000, 950), "now");
        // 60s → 1m.
        assert_eq!(format_age_since(1000, 940), "1m ago");
        // 10 minutes.
        assert_eq!(format_age_since(10_000, 9_400), "10m ago");
        // 1h ago
        assert_eq!(format_age_since(7200, 1), "1h ago");
        // 2d ago
        assert_eq!(format_age_since(3 * 86400, 86400), "2d ago");
    }

    #[test]
    fn render_history_entry_formats_state_transitions() {
        let entry = TaskHistoryInfo {
            field: "state".into(),
            old_value: Some("ready".into()),
            new_value: Some("active".into()),
            session_id: Some("s1".into()),
            created_at: 1,
        };
        let s = render_history_entry(&entry);
        assert!(s.contains("state"));
        assert!(s.contains("ready"));
        assert!(s.contains("active"));
        assert!(s.contains("\u{2192}"));
    }

    #[test]
    fn render_history_entry_formats_other_fields() {
        let entry = TaskHistoryInfo {
            field: "priority".into(),
            old_value: Some("3".into()),
            new_value: Some("7".into()),
            session_id: None,
            created_at: 1,
        };
        let s = render_history_entry(&entry);
        assert!(s.starts_with("priority:"));
        assert!(s.contains("3"));
        assert!(s.contains("7"));
    }

    #[test]
    fn primary_session_prefers_live_worker() {
        let task = make_task_info(1, "t", "active");
        let mut worker = make_task_session("worker", "s-worker");
        worker.is_live = true;
        let mut reviewer = make_task_session("reviewer", "s-rev");
        reviewer.is_live = true;
        let creator = make_task_session("creator", "s-creator");
        let picked = primary_session_id(&task, &[creator, reviewer, worker]);
        assert_eq!(picked.as_deref(), Some("s-worker"));
    }

    #[test]
    fn primary_session_falls_back_to_non_archived() {
        let task = make_task_info(1, "t", "active");
        let mut archived = make_task_session("worker", "s-archived");
        archived.archived = Some(true);
        let creator = make_task_session("creator", "s-creator");
        let picked = primary_session_id(&task, &[archived, creator]);
        assert_eq!(picked.as_deref(), Some("s-creator"));
    }

    #[test]
    fn primary_session_falls_back_to_task_session_id() {
        let task = make_task_info(1, "t", "active");
        // All sessions archived — fall back to task.session_id.
        let mut a = make_task_session("worker", "s-a");
        a.archived = Some(true);
        let picked = primary_session_id(&task, &[a]);
        // task.session_id = Some("s100") in make_task_info.
        assert_eq!(picked.as_deref(), Some("s100"));
    }

    #[test]
    fn render_task_detail_includes_sessions_and_history() {
        let mut app = make_app();
        let task = make_task_info(5, "Detail test", "active");
        let sessions = vec![
            make_task_session("worker", "s-w"),
            make_task_session("reviewer", "s-r"),
        ];
        let history = vec![TaskHistoryInfo {
            field: "state".into(),
            old_value: Some("ready".into()),
            new_value: Some("active".into()),
            session_id: Some("s-w".into()),
            created_at: now_secs() * 1000 - 60_000,
        }];
        app.render_task_detail(&task, &[], &[], &sessions, &history);
        let joined: String = app
            .messages
            .iter()
            .filter_map(|m| match m {
                MessageItem::Status { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Sessions (2"));
        assert!(joined.contains("History (1"));
        assert!(joined.contains("worker"));
        assert!(joined.contains("reviewer"));
        // State transition rendered.
        assert!(joined.contains("ready"));
        assert!(joined.contains("active"));
        // Age formatting present (should be "Nm ago" or "now").
        assert!(
            joined.contains(" ago") || joined.contains("now"),
            "joined: {joined}"
        );
    }

    #[test]
    fn task_switch_slash_command_sets_pending_and_requests_task_get() {
        let mut app = make_app();
        let action = app.handle_task_slash_command("switch 42");
        match action {
            Some(Action::TaskGet { id }) => assert_eq!(id, 42),
            other => panic!("expected TaskGet, got {other:?}"),
        }
        assert_eq!(app.pending_task_switch, Some(42));
    }

    #[test]
    fn task_switch_response_emits_switch_session() {
        let mut app = make_app();
        app.pending_task_switch = Some(7);
        let task = make_task_info(7, "t", "active");
        let mut worker = make_task_session("worker", "s-worker");
        worker.is_live = true;
        let action = app.handle_server_response(Response::TaskDetail {
            task,
            messages: vec![],
            relations: vec![],
            subtasks: vec![],
            sessions: vec![worker],
            history: vec![],
        });
        match action {
            Some(Action::SwitchSession(sid)) => assert_eq!(sid, "s-worker"),
            other => panic!("expected SwitchSession, got {other:?}"),
        }
        assert!(app.pending_task_switch.is_none());
    }

    #[test]
    fn task_switch_usage_error_on_missing_id() {
        let mut app = make_app();
        let action = app.handle_task_slash_command("switch");
        assert!(action.is_none());
        assert!(app.pending_task_switch.is_none());
        assert!(
            app.messages.iter().any(|m| matches!(
                m,
                MessageItem::Error { text } if text.contains("usage")
            )),
            "expected usage error"
        );
    }

    #[test]
    fn task_active_opens_picker_filtered_to_active() {
        let mut app = make_app();
        let action = app.handle_task_slash_command("active");
        match action {
            Some(Action::OpenTaskPickerWithState { state }) => {
                assert_eq!(state, "active");
            }
            other => panic!("expected OpenTaskPickerWithState, got {other:?}"),
        }
    }
}
