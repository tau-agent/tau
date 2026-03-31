//! Application state for the TUI.

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use tau::protocol::Response;
use tau::types::{AssistantContent, Message, StreamEvent, ToolResultMessage, UserContent};

use crate::events::Event;
use crate::message::MessageItem;
use crate::render::RendererRegistry;
use crate::theme::Theme;

/// Cumulative usage tracking (mirrored from tau-cli).
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
    pub fn add(&mut self, usage: &tau::Usage) {
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
    /// Server stream ended.
    pub server_done: bool,
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
            scroll_pos: std::cell::Cell::new(None),
            max_scroll: std::cell::Cell::new(0),

            totals: UsageTotals::default(),
            should_quit: false,
            textarea,
            spinner_frame: 0,
            last_escape: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .unwrap(),
            queued_messages: Vec::new(),
            history_index: None,
            history_saved_text: String::new(),
            pending_subscription_usage: false,
            server_done: false,
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
                    ..
                }) => {
                    let output = content
                        .iter()
                        .filter_map(|c| match c {
                            tau::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
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
                        duration: std::time::Duration::ZERO,
                    });
                }
                Message::CompactionSummary(_) => {
                    // Skip compaction summaries in the UI
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
                self.handle_server_response(response);
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
                    self.spinner_frame = self.spinner_frame.wrapping_add(1);
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
            // Everything else goes to textarea
            _ => {
                // Reset history browsing on any other key
                self.history_index = None;
                self.textarea.input(event_to_tui_textarea(key));
                None
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
            // All other keys go to textarea (compose steering message while streaming)
            _ => {
                self.textarea.input(event_to_tui_textarea(key));
                None
            }
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
            "/help" => {
                self.messages.push(MessageItem::Status {
                    text: "Commands: /status /model [id] /theme [name] /cwd [path] /help /quit"
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
            _ => {
                self.messages.push(MessageItem::Error {
                    text: format!("unknown command: {}. Type /help", cmd),
                });
                None
            }
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
                        duration: started_at.elapsed(),
                    };
                }
                _ => {}
            }
        }
        // Remove any remaining empty streaming placeholders.
        self.messages
            .retain(|m| !matches!(m, MessageItem::AssistantStreaming { text } if text.is_empty()));
    }

    fn handle_server_response(&mut self, response: Response) {
        match response {
            Response::Stream { event } => {
                // If we receive stream events while in Input mode,
                // another client is chatting — switch to streaming view.
                if self.mode == AppMode::Input {
                    self.mode = AppMode::Streaming;
                }
                self.handle_stream_event(*event);
            }
            Response::AgentDone => {
                self.finalize_in_flight();
                self.mode = AppMode::Input;
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
                self.mode = AppMode::Input;
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
                self.mode = AppMode::Input;
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
                let stats_str = tau::protocol::format_stats(&info.stats);
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
                use tau::auth::UsageBucket;
                let fmt_bucket = |b: &Option<UsageBucket>| -> String {
                    match b.as_ref().and_then(|b| b.utilization) {
                        Some(u) => format!("{:.0}%", u),
                        None => "?".into(),
                    }
                };
                let mut parts = Vec::new();
                if usage.five_hour.is_some() {
                    parts.push(format!("5h: {}", fmt_bucket(&usage.five_hour)));
                }
                if usage.seven_day.is_some() {
                    parts.push(format!("7d: {}", fmt_bucket(&usage.seven_day)));
                }
                if usage.seven_day_sonnet.is_some() {
                    parts.push(format!("sonnet: {}", fmt_bucket(&usage.seven_day_sonnet)));
                }
                if usage.seven_day_opus.is_some() {
                    parts.push(format!("opus: {}", fmt_bucket(&usage.seven_day_opus)));
                }
                if !parts.is_empty() {
                    self.messages.push(MessageItem::Status {
                        text: format!("usage: {}", parts.join(" | ")),
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_stream_event(&mut self, event: StreamEvent) {
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
                    name: tool_call.name,
                    args: tool_call.arguments,
                    output_lines: Vec::new(),
                    started_at: std::time::Instant::now(),
                });
            }
            StreamEvent::ToolOutputDelta { delta, .. } => {
                // Append output to the active tool
                if let Some(MessageItem::ToolActive { output_lines, .. }) = self.messages.last_mut()
                {
                    output_lines.push(delta);
                }
            }
            StreamEvent::ToolResult {
                tool_name,
                is_error,
                content,
                ..
            } => {
                // Replace active tool with completed tool
                if let Some(last @ MessageItem::ToolActive { .. }) = self.messages.last_mut() {
                    let (args, started_at) =
                        if let MessageItem::ToolActive {
                            args, started_at, ..
                        } = last
                        {
                            (args.clone(), *started_at)
                        } else {
                            (serde_json::Value::Null, std::time::Instant::now())
                        };
                    *last = MessageItem::ToolComplete {
                        name: tool_name,
                        args,
                        output: content,
                        is_error,
                        duration: started_at.elapsed(),
                    };
                } else {
                    self.messages.push(MessageItem::ToolComplete {
                        name: tool_name,
                        args: serde_json::Value::Null,
                        output: content,
                        is_error,
                        duration: std::time::Duration::ZERO,
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

                // Convert last streaming message to complete
                if let Some(item) = self.messages.last_mut()
                    && let MessageItem::AssistantStreaming { .. } = item
                {
                    *item = MessageItem::Assistant { text: final_text };
                }
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
                self.messages.push(MessageItem::Error { text: msg });
                self.mode = AppMode::Input;
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
}

/// Convert a crossterm KeyEvent to a tui_textarea compatible input event.
fn event_to_tui_textarea(key: &KeyEvent) -> crossterm::event::Event {
    crossterm::event::Event::Key(*key)
}
