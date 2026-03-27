//! Message items displayed in the chat viewport.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

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
    /// Tool call summary.
    Tool { name: String, preview: String },
    /// Status line (e.g. "[cancelled]", "[Working...]").
    Status { text: String },
    /// Error message.
    Error { text: String },
}

impl MessageItem {
    /// Render this item to ratatui `Text` for the given width.
    pub fn to_text(&self, _width: u16) -> Text<'static> {
        match self {
            MessageItem::User { text } => {
                let mut lines = Vec::new();
                lines.push(Line::from(""));
                // User label
                lines.push(Line::from(Span::styled(
                    " You ",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                // Message text with left padding
                for line in text.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
                Text::from(lines)
            }
            MessageItem::Assistant { text } | MessageItem::AssistantStreaming { text } => {
                let mut lines = Vec::new();
                lines.push(Line::from(""));
                // Assistant label
                lines.push(Line::from(Span::styled(
                    " Assistant ",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                // Message text with left padding
                for line in text.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
                // Streaming cursor
                if matches!(self, MessageItem::AssistantStreaming { .. })
                    && let Some(last) = lines.last_mut()
                {
                    let mut spans = vec![last.spans.drain(..).collect::<Vec<_>>()];
                    spans[0].push(Span::styled("▌", Style::default().fg(Color::Gray)));
                    *last = Line::from(spans.into_iter().flatten().collect::<Vec<_>>());
                }
                Text::from(lines)
            }
            MessageItem::Thinking { text, done } => {
                let style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
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
                let style = Style::default().fg(Color::Yellow);
                Text::from(Line::from(vec![
                    Span::styled("  [tool: ", style),
                    Span::styled(name.clone(), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}]", preview), style),
                ]))
            }
            MessageItem::Status { text } => Text::from(Line::from(Span::styled(
                format!("  {}", text),
                Style::default().fg(Color::DarkGray),
            ))),
            MessageItem::Error { text } => Text::from(Line::from(Span::styled(
                format!("  error: {}", text),
                Style::default().fg(Color::Red),
            ))),
        }
    }
}
