//! Message items displayed in the chat viewport.
//!
//! Pi-style rendering: message types are differentiated purely by background
//! color. No labels like "You" or "Assistant".
//!

/// Wrap a single text line at `max_width` characters.
/// Tries to break at word boundaries; falls back to hard break.
fn wrap_str(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || line.len() <= max_width {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut remaining = line;
    while remaining.len() > max_width {
        // Try to find a word boundary (space) to break at
        let break_at = remaining[..max_width]
            .rfind(' ')
            .map(|i| i + 1) // include the space on the current line
            .unwrap_or(max_width); // hard break if no space found
        result.push(remaining[..break_at].to_string());
        remaining = &remaining[break_at..];
    }
    if !remaining.is_empty() {
        result.push(remaining.to_string());
    }
    result
}

/// Wrap all lines of a text block to fit within `max_width`.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    text.lines().flat_map(|l| wrap_str(l, max_width)).collect()
}
// Note: messages do NOT include leading/trailing empty lines for spacing.
// The caller (ui.rs draw_messages) handles inter-message spacing.

use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};

use crate::render::RendererRegistry;
use crate::theme::Theme;

/// A single item in the chat history.
#[derive(Debug, Clone)]
pub enum MessageItem {
    /// User message.
    User { text: String },
    /// Complete assistant message.
    Assistant { text: String },
    /// Streaming assistant text (still arriving).
    AssistantStreaming { text: String },
    /// Thinking indicator / content.
    Thinking { text: String, done: bool },
    /// Tool actively running (output streaming in).
    ToolActive {
        name: String,
        args: serde_json::Value,
        output_lines: Vec<String>,
        started_at: std::time::Instant,
    },
    /// Tool execution completed.
    ToolComplete {
        name: String,
        args: serde_json::Value,
        output: String,
        is_error: bool,
        duration: std::time::Duration,
    },
    /// Status line (e.g. "[cancelled]", "[Working...]").
    Status { text: String },
    /// Error message.
    Error { text: String },
}

/// Pad each line to `width` so the background color fills the full row.
pub fn fill_bg(lines: &mut [Line<'static>], style: Style, width: u16) {
    for line in lines.iter_mut() {
        let visible = line.width();
        let pad = (width as usize).saturating_sub(visible);
        if pad > 0 {
            line.spans.push(Span::styled(" ".repeat(pad), style));
        }
        *line = line.clone().style(style);
    }
}

impl MessageItem {
    /// Render this item to ratatui `Text` for the given width.
    /// Does NOT include leading/trailing spacer lines — caller handles spacing.
    pub fn to_text(
        &self,
        width: u16,
        theme: &Theme,
        renderers: &RendererRegistry,
    ) -> Text<'static> {
        match self {
            MessageItem::User { text } => {
                let bg_style = theme.bg(theme.user_message_bg);
                let text_style = bg_style.fg(theme.user_message_text.to_ratatui());
                let usable = (width as usize).saturating_sub(1); // 1 char indent

                let mut lines: Vec<Line<'static>> = Vec::new();
                for l in wrap_text(text, usable) {
                    lines.push(Line::from(Span::styled(format!(" {}", l), text_style)));
                }
                fill_bg(&mut lines, bg_style, width);
                Text::from(lines)
            }
            MessageItem::Assistant { text } | MessageItem::AssistantStreaming { text } => {
                let usable = (width as usize).saturating_sub(1);
                let mut lines: Vec<Line<'static>> = Vec::new();
                for l in wrap_text(text, usable) {
                    lines.push(Line::from(format!(" {}", l)));
                }
                if lines.is_empty() {
                    lines.push(Line::from(""));
                }
                // Streaming cursor
                if matches!(self, MessageItem::AssistantStreaming { .. })
                    && let Some(last) = lines.last_mut()
                {
                    let existing: Vec<Span<'static>> = last.spans.drain(..).collect();
                    let mut spans = existing;
                    spans.push(Span::styled("▌", theme.fg(theme.muted)));
                    *last = Line::from(spans);
                }
                Text::from(lines)
            }
            MessageItem::Thinking { text, done } => {
                let style = theme.italic_fg(theme.thinking_text);
                if *done && text.is_empty() {
                    return Text::from("");
                }
                let label = if *done { " Thought" } else { " Thinking..." };
                let mut lines = vec![Line::from(Span::styled(label, style))];
                if !text.is_empty() {
                    let usable = (width as usize).saturating_sub(1);
                    for l in wrap_text(text, usable) {
                        lines.push(Line::from(Span::styled(format!(" {}", l), style)));
                    }
                }
                Text::from(lines)
            }
            MessageItem::ToolActive {
                name,
                args,
                output_lines,
                started_at,
            } => {
                let renderer = renderers.get(name);
                let lines =
                    renderer.render_active(name, args, output_lines, *started_at, theme, width);
                Text::from(lines)
            }
            MessageItem::ToolComplete {
                name,
                args,
                output,
                is_error,
                duration,
            } => {
                let renderer = renderers.get(name);
                let lines = renderer
                    .render_complete(name, args, output, *is_error, *duration, theme, width);
                Text::from(lines)
            }
            MessageItem::Status { text } => Text::from(Line::from(Span::styled(
                format!(" {}", text),
                theme.status_style(),
            ))),
            MessageItem::Error { text } => {
                let bg_style = theme.error_style();
                let mut lines = vec![
                    Line::from(Span::styled(" ", bg_style)),
                    Line::from(Span::styled(format!(" error: {}", text), bg_style)),
                    Line::from(Span::styled(" ", bg_style)),
                ];
                fill_bg(&mut lines, bg_style, width);
                Text::from(lines)
            }
        }
    }
}
