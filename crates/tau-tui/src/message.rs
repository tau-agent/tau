//! Message items displayed in the chat viewport.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

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
    /// Tool call summary (success).
    Tool { name: String, preview: String },
    /// Tool call that resulted in an error.
    ToolError { name: String, message: String },
    /// Status line (e.g. "[cancelled]", "[Working...]").
    Status { text: String },
    /// Error message.
    Error { text: String },
}

impl MessageItem {
    /// Render this item to ratatui `Text` for the given width.
    pub fn to_text(&self, _width: u16, theme: &Theme) -> Text<'static> {
        match self {
            MessageItem::User { text } => {
                let mut lines = Vec::new();
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(" You ", theme.user_label_style())));
                lines.push(Line::from(""));
                for line in text.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
                Text::from(lines)
            }
            MessageItem::Assistant { text } | MessageItem::AssistantStreaming { text } => {
                let mut lines = Vec::new();
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " Assistant ",
                    theme.assistant_label_style(),
                )));
                lines.push(Line::from(""));
                for line in text.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
                // Streaming cursor
                if matches!(self, MessageItem::AssistantStreaming { .. })
                    && let Some(last) = lines.last_mut()
                {
                    let mut spans = vec![last.spans.drain(..).collect::<Vec<_>>()];
                    spans[0].push(Span::styled("▌", theme.fg(theme.muted)));
                    *last = Line::from(spans.into_iter().flatten().collect::<Vec<_>>());
                }
                Text::from(lines)
            }
            MessageItem::Thinking { text, done } => {
                let style = theme.italic_fg(theme.thinking_text);
                if *done && text.is_empty() {
                    return Text::from("");
                }
                let label = if *done { "  Thought" } else { "  Thinking..." };
                let mut lines = vec![Line::from(Span::styled(label, style))];
                if !text.is_empty() {
                    for line in text.lines() {
                        lines.push(Line::from(Span::styled(format!("  {}", line), style)));
                    }
                }
                Text::from(lines)
            }
            MessageItem::Tool { name, preview } => {
                let style = theme.tool_success_style();
                let bold_style = style.add_modifier(Modifier::BOLD);
                Text::from(Line::from(vec![
                    Span::styled("  [tool: ", style),
                    Span::styled(name.clone(), bold_style),
                    Span::styled(format!(" {}]", preview), style),
                ]))
            }
            MessageItem::ToolError { name, message } => {
                let style = theme.tool_error_style();
                let bold_style = style.add_modifier(Modifier::BOLD);
                Text::from(Line::from(vec![
                    Span::styled("  [tool error: ", style),
                    Span::styled(name.clone(), bold_style),
                    Span::styled(format!(" {}]", message), style),
                ]))
            }
            MessageItem::Status { text } => Text::from(Line::from(Span::styled(
                format!("  {}", text),
                theme.status_style(),
            ))),
            MessageItem::Error { text } => Text::from(Line::from(Span::styled(
                format!("  error: {}", text),
                theme.error_style(),
            ))),
        }
    }
}
