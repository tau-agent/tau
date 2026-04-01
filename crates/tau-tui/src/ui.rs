//! Layout and rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use tau::protocol::format_tokens;

use tau::types::AgentPhase;

use crate::app::{App, AppMode};
use crate::theme::Theme;

/// Draw the full UI.
pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();

    // Layout: messages(flex) | input(dynamic) | footer(1)
    let input_height = input_area_height(app, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),               // messages
            Constraint::Length(input_height), // input area
            Constraint::Length(1),            // footer (stats left, model right)
        ])
        .split(area);

    draw_messages(frame, app, theme, chunks[0]);
    draw_input(frame, app, theme, chunks[1]);
    draw_footer(frame, app, theme, chunks[2]);
}

/// Height of the input area: visual lines (accounting for wrap) + 2 borders.
fn input_area_height(app: &App, width: u16) -> u16 {
    // Inner width: full width minus left/right (no side borders, but 1 char padding each side)
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let visual_lines: usize = app
        .textarea
        .lines()
        .iter()
        .map(|line| {
            let len = line.len().max(1); // empty line = 1 visual line
            len.div_ceil(inner_width)
        })
        .sum();
    ((visual_lines as u16) + 2).clamp(3, 12)
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Wrap a single Line into multiple lines at `width` character boundaries.
/// Preserves span styles across the split.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }
    let line_width = line.width();
    if line_width <= width {
        return vec![line];
    }

    // Flatten all spans into (char, Style) pairs, then re-chunk
    let style = line.style;
    let mut chars: Vec<(char, ratatui::style::Style)> = Vec::new();
    for span in &line.spans {
        let span_style = style.patch(span.style);
        for ch in span.content.chars() {
            chars.push((ch, span_style));
        }
    }

    let mut result = Vec::new();
    for chunk in chars.chunks(width) {
        // Group consecutive chars with same style into spans
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_text = String::new();
        let mut current_style = chunk[0].1;
        for &(ch, st) in chunk {
            if st == current_style {
                current_text.push(ch);
            } else {
                spans.push(Span::styled(current_text, current_style));
                current_text = String::new();
                current_text.push(ch);
                current_style = st;
            }
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(current_text, current_style));
        }
        result.push(Line::from(spans));
    }
    result
}

fn draw_messages(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Build all message lines with single empty line between messages
    let w = area.width as usize;
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for msg in &app.messages {
        let text = msg.to_text(area.width, theme, &app.renderers);
        let msg_lines: Vec<Line<'static>> = text.lines.into_iter().collect();
        if msg_lines.is_empty() {
            continue;
        }
        // Add separator before this message (if there's already content)
        if !all_lines.is_empty() {
            all_lines.push(Line::from(""));
        }
        // Pre-wrap long lines so Line count == visual row count
        for line in msg_lines {
            all_lines.extend(wrap_line(line, w));
        }
    }

    // Add working indicator if streaming
    if app.mode == AppMode::Streaming {
        let needs_indicator = !matches!(
            app.messages.last(),
            Some(crate::message::MessageItem::AssistantStreaming { .. })
        );
        if needs_indicator {
            if !all_lines.is_empty() {
                all_lines.push(Line::from(""));
            }
            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(app.spinner().to_string(), theme.spinner_style()),
                Span::styled(
                    format!(" {}", app.phase.label()),
                    theme.spinner_message_style(),
                ),
            ]));
        }
    }

    // Empty line above the input field
    all_lines.push(Line::from(""));

    // Use Line count directly (no Wrap on the Paragraph — we don't wrap).
    // Long lines are handled by the terminal / ratatui truncation.
    let total_lines = all_lines.len();
    let visible = area.height as usize;

    // Pad with empty lines so content is bottom-aligned (starts just above input)
    if total_lines < visible {
        let pad = visible - total_lines;
        let mut padded = vec![Line::from(""); pad];
        padded.append(&mut all_lines);
        all_lines = padded;
    }

    let total_lines = all_lines.len();

    // Calculate scroll: None = follow bottom, Some(pos) = pinned at pos from top
    let max_scroll = total_lines.saturating_sub(visible);
    app.max_scroll.set(max_scroll);
    let scroll = match app.scroll_pos.get() {
        None => max_scroll, // follow bottom
        Some(pos) => {
            let clamped = pos.min(max_scroll);
            // Auto-unpin if scrolled all the way to the bottom
            if clamped >= max_scroll {
                app.scroll_pos.set(None);
            }
            clamped
        }
    };

    let paragraph = Paragraph::new(Text::from(all_lines)).scroll((scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Input area
// ---------------------------------------------------------------------------

fn draw_input(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    // Only top and bottom borders, single line — borderMuted always (like pi)
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_set(border::PLAIN)
        .border_style(theme.input_border_style());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&app.textarea, inner);
}

// ---------------------------------------------------------------------------
// Footer: stats left-aligned, model right-aligned
// ---------------------------------------------------------------------------

fn draw_footer(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let totals = &app.totals;
    let dim = theme.fg(theme.dim);

    // Build left side: session ID + stats
    let mut left_parts: Vec<Span<'static>> = Vec::new();

    // Session ID
    left_parts.push(Span::styled(app.session_id.clone(), dim));

    if totals.input > 0 {
        left_parts.push(Span::styled(
            format!("↑{}", format_tokens(totals.input)),
            dim,
        ));
    }
    if totals.output > 0 {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        left_parts.push(Span::styled(
            format!("↓{}", format_tokens(totals.output)),
            dim,
        ));
    }
    if totals.cache_read > 0 {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        left_parts.push(Span::styled(
            format!("R{}", format_tokens(totals.cache_read)),
            dim,
        ));
    }
    if totals.cache_write > 0 {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        left_parts.push(Span::styled(
            format!("W{}", format_tokens(totals.cache_write)),
            dim,
        ));
    }
    if totals.cost > 0.0 || totals.is_subscription {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        let cost_str = if totals.is_subscription {
            format!("${:.3} (sub)", totals.cost)
        } else {
            format!("${:.3}", totals.cost)
        };
        left_parts.push(Span::styled(cost_str, dim));
    }
    if totals.context_window > 0 {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        match totals.context_tokens {
            Some(t) => {
                let pct = (t as f64 / totals.context_window as f64) * 100.0;
                let color = theme.context_color(pct);
                left_parts.push(Span::styled(
                    format!("{:.1}%/{}", pct, format_tokens(totals.context_window)),
                    theme.fg(color),
                ));
            }
            None => {
                left_parts.push(Span::styled(
                    format!("?/{}", format_tokens(totals.context_window)),
                    dim,
                ));
            }
        }
    }

    // Build right side: model name + connection status
    let right_text = if app.server_done {
        format!("{}/{} (disconnected)", app.provider, app.model)
    } else if app.phase != AgentPhase::Idle {
        format!("{}/{} ({})", app.provider, app.model, app.phase.label())
    } else {
        format!("{}/{}", app.provider, app.model)
    };
    let right_style = if app.server_done {
        theme.fg(theme.error)
    } else {
        dim
    };
    let right_span = Span::styled(right_text.clone(), right_style);

    // Calculate left width
    let left_width: usize = left_parts.iter().map(|s| s.content.len()).sum();
    let right_width = right_text.len();
    let w = area.width as usize;

    // Padding between left and right
    let pad = w.saturating_sub(1 + left_width + 1 + right_width + 1);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" ")); // left margin
    spans.extend(left_parts);
    spans.push(Span::raw(" ".repeat(pad.max(1))));
    spans.push(right_span);
    spans.push(Span::raw(" ")); // right margin

    let footer_line = Line::from(spans);
    frame.render_widget(Paragraph::new(footer_line), area);
}
