//! Layout and rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use tau::protocol::format_tokens;

use crate::app::{App, AppMode};

/// Draw the full UI.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Layout: header(1) | messages(flex) | stats(1) | input(dynamic)
    let input_height = input_area_height(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // header
            Constraint::Min(3),               // messages
            Constraint::Length(1),            // stats bar
            Constraint::Length(input_height), // input area
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    draw_messages(frame, app, chunks[1]);
    draw_stats(frame, app, chunks[2]);
    draw_input(frame, app, chunks[3]);
}

/// Height of the input area: textarea lines + 2 (border).
fn input_area_height(app: &App) -> u16 {
    let lines = app.textarea.lines().len() as u16;
    (lines + 2).clamp(3, 10)
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let session_short = if app.session_id.len() > 8 {
        &app.session_id[..8]
    } else {
        &app.session_id
    };

    let header = Line::from(vec![
        Span::styled(
            " tau",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} · {}/{}", session_short, app.provider, app.model),
            Style::default().fg(Color::DarkGray),
        ),
        // Right-align mode indicator
        Span::raw(
            " ".repeat(
                area.width
                    .saturating_sub(
                        4 + 3
                            + session_short.len() as u16
                            + 3
                            + app.provider.len() as u16
                            + 1
                            + app.model.len() as u16
                            + 2,
                    )
                    .into(),
            ),
        ),
        match app.mode {
            AppMode::Input => Span::styled("●", Style::default().fg(Color::Green)),
            AppMode::Streaming => Span::styled(app.spinner(), Style::default().fg(Color::Yellow)),
        },
        Span::raw(" "),
    ]);

    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(Color::Rgb(30, 30, 40))),
        area,
    );
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

fn draw_messages(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Build all message lines
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for msg in &app.messages {
        let text = msg.to_text(area.width);
        for line in text.lines {
            all_lines.push(line);
        }
    }

    // Add working indicator if streaming
    if app.mode == AppMode::Streaming {
        // Only add if last message isn't already a streaming one
        let needs_indicator = !matches!(
            app.messages.last(),
            Some(crate::message::MessageItem::AssistantStreaming { .. })
        );
        if needs_indicator {
            all_lines.push(Line::from(""));
            all_lines.push(Line::from(Span::styled(
                format!("  {} Working...", app.spinner()),
                Style::default().fg(Color::Yellow),
            )));
        }
    }

    // Pad with empty lines so content is bottom-aligned (starts just above input)
    let content_lines = all_lines.len() as u16;
    let visible = area.height;
    if content_lines < visible {
        let pad = visible - content_lines;
        let mut padded = vec![Line::from(""); pad as usize];
        padded.append(&mut all_lines);
        all_lines = padded;
    }

    let total_lines = all_lines.len() as u16;

    // Calculate scroll position (scroll_offset=0 means bottom)
    let max_scroll = total_lines.saturating_sub(visible);
    let scroll = max_scroll.saturating_sub(app.scroll_offset.min(max_scroll));

    let paragraph = Paragraph::new(Text::from(all_lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);

    // Scrollbar
    if total_lines > visible {
        let mut scrollbar_state = ScrollbarState::new(max_scroll as usize)
            .position((max_scroll - scroll.min(max_scroll)) as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

// ---------------------------------------------------------------------------
// Stats bar
// ---------------------------------------------------------------------------

fn draw_stats(frame: &mut Frame, app: &App, area: Rect) {
    let totals = &app.totals;
    let mut parts = Vec::new();

    if totals.input > 0 {
        parts.push(Span::styled(
            format!("↑{}", format_tokens(totals.input)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if totals.output > 0 {
        parts.push(Span::styled(
            format!(" ↓{}", format_tokens(totals.output)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if totals.cache_read > 0 {
        parts.push(Span::styled(
            format!(" R{}", format_tokens(totals.cache_read)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if totals.cache_write > 0 {
        parts.push(Span::styled(
            format!(" W{}", format_tokens(totals.cache_write)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if totals.cost > 0.0 || totals.is_subscription {
        let cost_str = if totals.is_subscription {
            format!(" ${:.3} (sub)", totals.cost)
        } else {
            format!(" ${:.3}", totals.cost)
        };
        parts.push(Span::styled(cost_str, Style::default().fg(Color::DarkGray)));
    }
    if totals.context_window > 0 {
        let ctx = match totals.context_tokens {
            Some(t) => {
                let pct = (t as f64 / totals.context_window as f64) * 100.0;
                let color = if pct > 90.0 {
                    Color::Red
                } else if pct > 70.0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                };
                Span::styled(
                    format!(" {:.1}%/{}", pct, format_tokens(totals.context_window)),
                    Style::default().fg(color),
                )
            }
            None => Span::styled(
                format!(" ?/{}", format_tokens(totals.context_window)),
                Style::default().fg(Color::DarkGray),
            ),
        };
        parts.push(ctx);
    }

    // If nothing to show, add a dim placeholder
    if parts.is_empty() {
        parts.push(Span::styled(" ready", Style::default().fg(Color::DarkGray)));
    }

    // Prepend a space
    parts.insert(0, Span::raw(" "));

    let stats_line = Line::from(parts);
    frame.render_widget(
        Paragraph::new(stats_line).style(Style::default().bg(Color::Rgb(25, 25, 35))),
        area,
    );
}

// ---------------------------------------------------------------------------
// Input area
// ---------------------------------------------------------------------------

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(match app.mode {
            AppMode::Input => Style::default().fg(Color::Cyan),
            AppMode::Streaming => Style::default().fg(Color::DarkGray),
        })
        .title(match app.mode {
            AppMode::Input => Span::styled(
                " tau ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            AppMode::Streaming => Span::styled(" tau ", Style::default().fg(Color::DarkGray)),
        });

    // Render block first, then textarea inside
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&app.textarea, inner);
}
