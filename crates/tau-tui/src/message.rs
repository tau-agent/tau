//! Message items displayed in the chat viewport.
//!
//! Pi-style rendering: message types are differentiated purely by background
//! color. No labels like "You" or "Assistant".
//!
//! Note: messages do NOT include leading/trailing empty lines for spacing.
//! The caller (ui.rs draw_messages) handles inter-message spacing.

use ratatui::style::{Modifier, Style};
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
    /// Tool call (pending execution).
    ToolPending { name: String, preview: String },
    /// Tool call summary (success).
    Tool { name: String, preview: String },
    /// Tool call that resulted in an error.
    ToolError { name: String, message: String },
    /// Status line (e.g. "[cancelled]", "[Working...]").
    Status { text: String },
    /// Error message.
    Error { text: String },
}

/// Pad each line to `width` so the background color fills the full row.
fn fill_bg(lines: &mut [Line<'static>], style: Style, width: u16) {
    for line in lines.iter_mut() {
        let visible: usize = line.spans.iter().map(|s| s.content.len()).sum();
        let pad = (width as usize).saturating_sub(visible);
        if pad > 0 {
            line.spans.push(Span::styled(" ".repeat(pad), style));
        }
        *line = line.clone().style(style);
    }
}

fn tool_block(
    bg_style: Style,
    title_style: Style,
    name: &str,
    detail: &str,
    detail_style: Style,
    width: u16,
) -> Text<'static> {
    let mut lines = vec![
        Line::from(Span::styled(" ", bg_style)), // top padding
        Line::from(vec![
            Span::styled(format!(" {}", name), title_style),
            Span::styled(format!(" {}", detail), detail_style),
        ]),
        Line::from(Span::styled(" ", bg_style)), // bottom padding
    ];
    fill_bg(&mut lines, bg_style, width);
    Text::from(lines)
}

impl MessageItem {
    /// Render this item to ratatui `Text` for the given width.
    /// Does NOT include leading/trailing spacer lines — caller handles spacing.
    pub fn to_text(&self, width: u16, theme: &Theme) -> Text<'static> {
        match self {
            MessageItem::User { text } => {
                let bg_style = theme.bg(theme.user_message_bg);
                let text_style = bg_style.fg(theme.user_message_text.to_ratatui());

                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled(" ", bg_style))); // top padding
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(format!(" {}", l), text_style)));
                }
                lines.push(Line::from(Span::styled(" ", bg_style))); // bottom padding
                fill_bg(&mut lines, bg_style, width);
                Text::from(lines)
            }
            MessageItem::Assistant { text } | MessageItem::AssistantStreaming { text } => {
                let mut lines: Vec<Line<'static>> = Vec::new();
                for l in text.lines() {
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
                    for l in text.lines() {
                        lines.push(Line::from(Span::styled(format!(" {}", l), style)));
                    }
                }
                Text::from(lines)
            }
            MessageItem::ToolPending { name, preview } => {
                let bg_style = theme.tool_pending_style();
                let title_style = bg_style
                    .fg(theme.tool_title.to_ratatui())
                    .add_modifier(Modifier::BOLD);
                tool_block(
                    bg_style,
                    title_style,
                    name,
                    preview,
                    bg_style.fg(theme.tool_output.to_ratatui()),
                    width,
                )
            }
            MessageItem::Tool { name, preview } => {
                let bg_style = theme.tool_success_style();
                let title_style = bg_style
                    .fg(theme.tool_title.to_ratatui())
                    .add_modifier(Modifier::BOLD);
                tool_block(
                    bg_style,
                    title_style,
                    name,
                    preview,
                    bg_style.fg(theme.tool_output.to_ratatui()),
                    width,
                )
            }
            MessageItem::ToolError { name, message } => {
                let bg_style = theme.tool_error_style();
                let title_style = bg_style
                    .fg(theme.error.to_ratatui())
                    .add_modifier(Modifier::BOLD);
                tool_block(
                    bg_style,
                    title_style,
                    name,
                    message,
                    bg_style.fg(theme.tool_output.to_ratatui()),
                    width,
                )
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
