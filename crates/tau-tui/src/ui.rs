//! Layout and rendering.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
// styles come from Theme
use ratatui::Frame;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use tau::protocol::format_tokens;

use crate::app::{App, AppMode};
use crate::theme::Theme;

/// Draw the full UI.
pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
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

    draw_header(frame, app, theme, chunks[0]);
    draw_messages(frame, app, theme, chunks[1]);
    draw_stats(frame, app, theme, chunks[2]);
    draw_input(frame, app, theme, chunks[3]);
}

/// Height of the input area: textarea lines + 2 (border).
fn input_area_height(app: &App) -> u16 {
    let lines = app.textarea.lines().len() as u16;
    (lines + 2).clamp(3, 10)
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let session_short = if app.session_id.len() > 8 {
        &app.session_id[..8]
    } else {
        &app.session_id
    };

    let header = Line::from(vec![
        Span::styled(" tau", theme.bold_fg(theme.accent)),
        Span::styled(
            format!(" · {} · {}/{}", session_short, app.provider, app.model),
            theme.fg(theme.dim),
        ),
        // Flexible padding
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
            AppMode::Input => Span::styled("●", theme.fg(theme.success)),
            AppMode::Streaming => Span::styled(app.spinner(), theme.spinner_style()),
        },
        Span::raw(" "),
    ]);

    frame.render_widget(Paragraph::new(header).style(theme.header_style()), area);
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

fn draw_messages(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Build all message lines
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for msg in &app.messages {
        let text = msg.to_text(area.width, theme);
        for line in text.lines {
            all_lines.push(line);
        }
    }

    // Add working indicator if streaming
    if app.mode == AppMode::Streaming {
        let needs_indicator = !matches!(
            app.messages.last(),
            Some(crate::message::MessageItem::AssistantStreaming { .. })
        );
        if needs_indicator {
            all_lines.push(Line::from(""));
            all_lines.push(Line::from(Span::styled(
                format!("  {} Working...", app.spinner()),
                theme.spinner_style(),
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
            Scrollbar::new(ScrollbarOrientation::VerticalRight).style(theme.scrollbar_style()),
            area,
            &mut scrollbar_state,
        );
    }
}

// ---------------------------------------------------------------------------
// Stats bar
// ---------------------------------------------------------------------------

fn draw_stats(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let totals = &app.totals;
    let mut parts = Vec::new();

    let dim = theme.fg(theme.dim);

    if totals.input > 0 {
        parts.push(Span::styled(
            format!("↑{}", format_tokens(totals.input)),
            dim,
        ));
    }
    if totals.output > 0 {
        parts.push(Span::styled(
            format!(" ↓{}", format_tokens(totals.output)),
            dim,
        ));
    }
    if totals.cache_read > 0 {
        parts.push(Span::styled(
            format!(" R{}", format_tokens(totals.cache_read)),
            dim,
        ));
    }
    if totals.cache_write > 0 {
        parts.push(Span::styled(
            format!(" W{}", format_tokens(totals.cache_write)),
            dim,
        ));
    }
    if totals.cost > 0.0 || totals.is_subscription {
        let cost_str = if totals.is_subscription {
            format!(" ${:.3} (sub)", totals.cost)
        } else {
            format!(" ${:.3}", totals.cost)
        };
        parts.push(Span::styled(cost_str, dim));
    }
    if totals.context_window > 0 {
        let ctx = match totals.context_tokens {
            Some(t) => {
                let pct = (t as f64 / totals.context_window as f64) * 100.0;
                let color = theme.context_color(pct);
                Span::styled(
                    format!(" {:.1}%/{}", pct, format_tokens(totals.context_window)),
                    theme.fg(color),
                )
            }
            None => Span::styled(format!(" ?/{}", format_tokens(totals.context_window)), dim),
        };
        parts.push(ctx);
    }

    if parts.is_empty() {
        parts.push(Span::styled(" ready", dim));
    }

    parts.insert(0, Span::raw(" "));

    let stats_line = Line::from(parts);
    frame.render_widget(Paragraph::new(stats_line).style(theme.stats_style()), area);
}

// ---------------------------------------------------------------------------
// Input area
// ---------------------------------------------------------------------------

fn draw_input(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let (border_style, title_style) = match app.mode {
        AppMode::Input => (theme.input_border_active(), theme.bold_fg(theme.accent)),
        AppMode::Streaming => (theme.input_border_inactive(), theme.fg(theme.border_muted)),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(" tau ", title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&app.textarea, inner);
}
