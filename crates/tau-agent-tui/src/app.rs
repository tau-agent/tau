//! Application state for the TUI.

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use tau_agent::auth::SubscriptionUsage;
use tau_agent::protocol::{Response, SessionInfo};
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
}

/// A steering message is sent as the next turn right after the current one.
/// A queued message is sent after all steering messages.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub text: String,
    pub is_steering: bool,
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
    /// Messages queued while the agent is working.
    /// Steering messages (is_steering=true) are drained first, then queued.
    pub queued_messages: Vec<QueuedMessage>,
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
    /// Whether a text block in the current turn has already been
    /// finalized by a `StreamEvent::TextEnd`.  Used by `StreamEvent::Done`
    /// to suppress its fallback append branch when `TextEnd` has already
    /// converted the in-flight placeholder to its final `Assistant` form
    /// — without this flag the fallback would duplicate every text-only
    /// turn (regression from task #421).  Reset at the end of each turn
    /// and by `Start`/`Error`.
    pub turn_text_finalized: bool,
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
            queued_messages: Vec::new(),
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
            picker_sessions: Vec::new(),
            picker_cursor: 0,
            picker_confirm_delete: None,
            picker_confirm_archive: None,
            picker_previous_mode: AppMode::Input,
            picker_filter: String::new(),
            picker_filter_mode: false,
            picker_edit_tagline: None,
            picker_folded: std::collections::HashSet::new(),
            turn_text_finalized: false,
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

    /// Get user message history (most recent last, owned strings).
    fn user_history(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter_map(|m| match m {
                MessageItem::User { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
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
                self.handle_server_response(*response);
                // Fetch subscription usage if requested
                if self.pending_subscription_usage {
                    self.pending_subscription_usage = false;
                    return Some(Action::GetSubscriptionUsage);
                }
                // After AgentDone, drain steering messages first, then queued
                while self.mode == AppMode::Input && !self.queued_messages.is_empty() {
                    // Steering messages first, then queued
                    let idx = self
                        .queued_messages
                        .iter()
                        .position(|m| m.is_steering)
                        .unwrap_or(0);
                    let next = self.queued_messages.remove(idx);

                    // Handle slash commands from queue without sending to LLM
                    if next.text.starts_with('/') {
                        let action = self.handle_slash_command(&next.text);
                        if action.is_some() {
                            return action;
                        }
                        continue;
                    }

                    // Don't add user message locally — it arrives via Subscribe broadcast
                    self.scroll_to_bottom();
                    self.mode = AppMode::Streaming;
                    return Some(Action::SendQueued(next.text));
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
                    return self.handle_slash_command(&text);
                }

                // Don't add user message locally — it arrives via Subscribe broadcast
                self.scroll_to_bottom();
                self.mode = AppMode::Streaming;
                self.history_index = None;
                Some(Action::SendChat(text))
            }
            // Alt+Enter: queue message (sent after current turn)
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
                    return self.handle_slash_command(&text);
                }

                self.queued_messages.push(QueuedMessage {
                    text: text.clone(),
                    is_steering: false,
                });
                self.messages.push(MessageItem::Status {
                    text: format!("[queued: {}]", text),
                });
                None
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
                    let history = self.user_history();
                    if history.is_empty() {
                        return None;
                    }
                    let new_idx = match self.history_index {
                        None => {
                            // Save current text before browsing
                            self.history_saved_text = self.textarea.lines().join("\n");
                            history.len() - 1
                        }
                        Some(i) if i > 0 => i - 1,
                        Some(_) => return None, // already at oldest
                    };
                    self.history_index = Some(new_idx);
                    self.set_textarea_text(&history[new_idx]);
                    return None;
                }
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
            // Down arrow: browse history forward or restore saved text
            (KeyCode::Down, KeyModifiers::NONE) => {
                if let Some(idx) = self.history_index {
                    let history = self.user_history();
                    if idx + 1 < history.len() {
                        self.history_index = Some(idx + 1);
                        self.set_textarea_text(&history[idx + 1]);
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
            // TAB: open session picker
            (KeyCode::Tab, _) => Some(Action::OpenSessionPicker),
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

        self.picker_sessions
            .iter()
            .enumerate()
            .filter(|(idx, s)| {
                if hidden.contains(idx) {
                    return false;
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
                    return self.handle_slash_command(&text);
                }

                self.scroll_to_bottom();
                self.history_index = None;
                Some(Action::Steer(text))
            }
            // Alt+Enter during streaming: queued message (runs after steering)
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
                    return self.handle_slash_command(&text);
                }

                self.queued_messages.push(QueuedMessage {
                    text: text.clone(),
                    is_steering: false,
                });
                self.messages.push(MessageItem::Status {
                    text: format!("[queued: {}]", text),
                });
                None
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
            // TAB: open session picker
            (KeyCode::Tab, _) => Some(Action::OpenSessionPicker),
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
        self.totals = UsageTotals::default();
        self.totals.context_window = info.stats.context_window;
        self.totals.is_subscription = info.stats.is_subscription;
        self.messages.clear();
        self.restore_messages(&messages);
        self.scroll_to_bottom();
        self.mode = AppMode::Input;
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
            self.scroll_to_bottom();
            self.mode = AppMode::Input;
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
                    Some(parts[1])
                } else {
                    None
                };
                match self.run_task_list(state_filter) {
                    Ok(()) => {}
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task list: {}", e),
                        });
                    }
                }
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
                match self.run_task_get(id) {
                    Ok(()) => {}
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task get: {}", e),
                        });
                    }
                }
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
                match self.run_task_state_change(id, "approved") {
                    Ok(()) => {
                        return Some(Action::FireHook {
                            name: "task_state_changed".into(),
                            data: serde_json::json!({"task_id": id, "new_state": "approved"}),
                        });
                    }
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task approve: {}", e),
                        });
                    }
                }
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
                match self.run_task_state_change(id, "ready") {
                    Ok(()) => {
                        return Some(Action::FireHook {
                            name: "task_state_changed".into(),
                            data: serde_json::json!({"task_id": id, "new_state": "ready"}),
                        });
                    }
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task ready: {}", e),
                        });
                    }
                }
            }
            "create" => {
                let title = args.strip_prefix("create").unwrap_or("").trim();
                if title.is_empty() {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task create <title>".into(),
                    });
                    return None;
                }
                match self.run_task_create(title) {
                    Ok(()) => {}
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task create: {}", e),
                        });
                    }
                }
            }
            "search" => {
                let query = args.strip_prefix("search").unwrap_or("").trim();
                if query.is_empty() {
                    self.messages.push(MessageItem::Error {
                        text: "usage: /task search <query>".into(),
                    });
                    return None;
                }
                match self.run_task_search(query) {
                    Ok(()) => {}
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task search: {}", e),
                        });
                    }
                }
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
                match self.run_task_claim(id) {
                    Ok(()) => {}
                    Err(e) => {
                        self.messages.push(MessageItem::Error {
                            text: format!("task claim: {}", e),
                        });
                    }
                }
            }
            "status" | "queue" => match self.run_task_status() {
                Ok(()) => {}
                Err(e) => {
                    self.messages.push(MessageItem::Error {
                        text: format!("task status: {}", e),
                    });
                }
            },
            "mq" => match self.run_task_merge_queue() {
                Ok(()) => {}
                Err(e) => {
                    self.messages.push(MessageItem::Error {
                        text: format!("task mq: {}", e),
                    });
                }
            },
            _ => {
                self.messages.push(MessageItem::Error {
                    text: format!(
                        "unknown task command: {}. Use: list [state], get <id>, create <title>, search <query>, claim <id>, approve <id>, ready <id>, status, mq",
                        subcmd
                    ),
                });
            }
        }
        None
    }

    /// Get the project path for task DB queries.
    /// Uses the current session's cwd if available, falls back to process cwd.
    fn task_project(&self) -> String {
        if let Some(ref cwd) = self.session_cwd {
            return cwd.clone();
        }
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    }

    fn run_task_status(&mut self) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let project = self.task_project();
        let status = tau_agent::tasks_scheduler::get_status(&db, &project)?;
        let output = tau_agent::tasks_scheduler::format_status(&status);
        self.messages.push(MessageItem::Status { text: output });
        Ok(())
    }

    fn run_task_merge_queue(&mut self) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let project = self.task_project();
        let approved = db.list_tasks(&project, Some("approved"), None, None, None)?;
        let merging = db.list_tasks(&project, Some("merging"), None, None, None)?;
        if approved.is_empty() && merging.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "merge queue is empty".into(),
            });
            return Ok(());
        }
        self.messages.push(MessageItem::Status {
            text: "  MERGE QUEUE".into(),
        });
        self.messages.push(MessageItem::Status {
            text: format!("  {:>4}  {:<12}  {:<14}  TITLE", "ID", "STATE", "BRANCH"),
        });
        for t in approved.iter().chain(merging.iter()) {
            let branch = t.branch.as_deref().unwrap_or("-");
            self.messages.push(MessageItem::Status {
                text: format!(
                    "  {:>4}  {:<12}  {:<14}  {}",
                    t.id, t.state, branch, t.title
                ),
            });
        }
        Ok(())
    }

    fn run_task_list(&mut self, state_filter: Option<&str>) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let project = self.task_project();
        let tasks = db.list_tasks(&project, state_filter, None, None, None)?;
        if tasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "no tasks".into(),
            });
            return Ok(());
        }
        let tree = tau_agent::tasks_db::tree_order(tasks);
        self.messages.push(MessageItem::Status {
            text: format!(
                "  {:>4}  {:<12}  {:>8}  {:<8}  TITLE",
                "ID", "STATE", "PRIORITY", "SESSION"
            ),
        });
        for (depth, t) in &tree {
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
        Ok(())
    }

    fn run_task_get(&mut self, id: i64) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let task = db
            .get_task(id)?
            .ok_or_else(|| tau_agent::Error::Io(format!("task {} not found", id)))?;

        let skip = if task.skip_review { "yes" } else { "no" };
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
                "State: {} | Priority: {} | Skip review: {}",
                task.state, task.priority, skip
            ),
        });
        self.messages.push(MessageItem::Status {
            text: format!("Branch: {} | Parent: {}", branch, parent),
        });

        // Messages
        let messages = db.get_messages(id)?;
        if !messages.is_empty() {
            self.messages.push(MessageItem::Status {
                text: format!("Messages: {}", messages.len()),
            });
            for msg in &messages {
                let author = msg.author.as_deref().unwrap_or("unknown");
                let preview: String = msg.content.chars().take(80).collect();
                let ellipsis = if msg.content.len() > 80 { "..." } else { "" };
                self.messages.push(MessageItem::Status {
                    text: format!("  #{} [{}] {}{}", msg.id, author, preview, ellipsis),
                });
            }
        }

        // Subtasks
        let subtasks = db.get_subtasks(id)?;
        if !subtasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "Subtasks:".into(),
            });
            for st in &subtasks {
                let text = format!("  #{:<4} {:<8} {}", st.id, st.state, st.title);
                if st.state == "failed" {
                    self.messages.push(MessageItem::Error { text });
                } else {
                    self.messages.push(MessageItem::Status { text });
                }
            }
        }

        Ok(())
    }

    fn run_task_create(&mut self, title: &str) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let project = self.task_project();
        let task = db.create_task(&project, title, None, None, None, false, false, false)?;
        self.messages.push(MessageItem::Status {
            text: format!("Created task #{}: {}", task.id, task.title),
        });
        Ok(())
    }

    fn run_task_search(&mut self, query: &str) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let project = self.task_project();
        let tasks = db.search_tasks(&project, query, None)?;
        if tasks.is_empty() {
            self.messages.push(MessageItem::Status {
                text: "no matching tasks".into(),
            });
            return Ok(());
        }
        self.messages.push(MessageItem::Status {
            text: format!(
                "  {:>4}  {:<12}  {:>8}  {:<8}  TITLE",
                "ID", "STATE", "PRIORITY", "SESSION"
            ),
        });
        for t in &tasks {
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
        Ok(())
    }

    fn run_task_state_change(&mut self, id: i64, new_state: &str) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let update = tau_agent::tasks_db::TaskUpdate {
            state: Some(new_state.to_string()),
            ..Default::default()
        };
        let task = db.update_task(id, &update, None)?;
        self.messages.push(MessageItem::Status {
            text: format!("task #{} → {} : {}", task.id, task.state, task.title),
        });
        Ok(())
    }

    fn run_task_claim(&mut self, id: i64) -> tau_agent::Result<()> {
        let db = tau_agent::tasks_db::TasksDb::open_default()?;
        let result = db.assign_task(id, &self.session_id)?;
        self.messages.push(MessageItem::Status {
            text: format!("Claimed task #{}: {}", result.task.id, result.task.title),
        });
        Ok(())
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

    fn handle_server_response(&mut self, response: Response) {
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
                    } else {
                        self.mode
                    };
                    if effective == AppMode::Streaming {
                        self.finalize_in_flight();
                        self.set_mode(AppMode::Input);
                    }
                    self.phase = AgentPhase::Idle;
                    return;
                }
                // If we receive stream events while in Input mode,
                // another client is chatting — switch to streaming view.
                let effective = if self.mode == AppMode::SessionPicker {
                    self.picker_previous_mode
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
                    return;
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
            _ => {}
        }
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
                    };
                } else {
                    self.messages.push(MessageItem::ToolComplete {
                        name: tool_name,
                        args: serde_json::Value::Null,
                        output: content,
                        is_error,
                        duration: None,
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
                self.messages.push(MessageItem::Status { text: message });
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
    /// Send the next queued message (after AgentDone).
    SendQueued(String),
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
}

/// Convert a crossterm KeyEvent to a tui_textarea compatible input event.
fn event_to_tui_textarea(key: &KeyEvent) -> crossterm::event::Event {
    crossterm::event::Event::Key(*key)
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
}
