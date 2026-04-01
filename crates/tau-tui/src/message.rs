//! Message items displayed in the chat viewport.
//!
//! Pi-style rendering: message types are differentiated purely by background
//! color. No labels like "You" or "Assistant".
//!

/// Wrap a single text line at `max_width` display columns.
/// Uses unicode display width so multi-byte chars (box drawing, CJK, etc.) work.
/// Tries to break at word boundaries; falls back to hard break.
fn wrap_str(line: &str, max_width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    if max_width == 0 || UnicodeWidthStr::width(line) <= max_width {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut remaining = line;
    while UnicodeWidthStr::width(remaining) > max_width {
        // Walk chars to find the byte offset where display width exceeds max_width
        let mut width_so_far = 0;
        let mut last_space_byte = None;
        let mut cut_byte = remaining.len();
        for (byte_idx, ch) in remaining.char_indices() {
            let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width_so_far + ch_w > max_width {
                cut_byte = byte_idx;
                break;
            }
            width_so_far += ch_w;
            if ch == ' ' {
                last_space_byte = Some(byte_idx + 1); // break after the space
            }
        }
        let break_at = last_space_byte.unwrap_or(cut_byte);
        // Avoid zero-progress infinite loop (e.g. single wide char > max_width)
        let break_at = if break_at == 0 {
            remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len())
        } else {
            break_at
        };
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
                lines.push(Line::from(Span::styled(" ", bg_style))); // top padding
                for l in wrap_text(text, usable) {
                    lines.push(Line::from(Span::styled(format!(" {}", l), text_style)));
                }
                lines.push(Line::from(Span::styled(" ", bg_style))); // bottom padding
                fill_bg(&mut lines, bg_style, width);
                Text::from(lines)
            }
            MessageItem::Assistant { text } | MessageItem::AssistantStreaming { text } => {
                let usable = (width as usize).saturating_sub(1);
                let trimmed = text.trim_start_matches('\n');
                let mut lines: Vec<Line<'static>> = Vec::new();
                for l in wrap_text(trimmed, usable) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_str_no_wrap_needed() {
        assert_eq!(wrap_str("hello", 10), vec!["hello"]);
        assert_eq!(wrap_str("hello", 5), vec!["hello"]);
    }

    #[test]
    fn wrap_str_empty() {
        assert_eq!(wrap_str("", 10), vec![""]);
    }

    #[test]
    fn wrap_str_zero_width() {
        assert_eq!(wrap_str("hello world", 0), vec!["hello world"]);
    }

    #[test]
    fn wrap_str_word_boundary() {
        let result = wrap_str("hello world foo", 11);
        assert_eq!(result, vec!["hello ", "world foo"]);
    }

    #[test]
    fn wrap_str_hard_break() {
        let result = wrap_str("abcdefghij", 5);
        assert_eq!(result, vec!["abcde", "fghij"]);
    }

    #[test]
    fn wrap_str_cjk_double_width() {
        let result = wrap_str("\u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94}", 6);
        assert_eq!(result, vec!["\u{4e00}\u{4e8c}\u{4e09}", "\u{56db}\u{4e94}"]);
    }

    #[test]
    fn wrap_str_box_drawing() {
        let line = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";
        let result = wrap_str(line, 4);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].chars().count(), 4);
        assert_eq!(result[1].chars().count(), 4);
    }

    #[test]
    fn wrap_str_mixed_ascii_cjk() {
        let result = wrap_str("hi\u{4e00}\u{4e8c}", 4);
        assert_eq!(result, vec!["hi\u{4e00}", "\u{4e8c}"]);
    }

    #[test]
    fn wrap_text_multiline() {
        let result = wrap_text("aaa\nbbb\nccc", 10);
        assert_eq!(result, vec!["aaa", "bbb", "ccc"]);
    }
}
