//! Layout and rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use tau_agent::protocol::format_tokens;
use tau_agent::types::AgentPhase;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppMode};
use crate::theme::{Theme, ThemeColor};

/// Draw the full UI.
pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();

    // Layout: messages(flex) | steer(0 or 1) | input(dynamic) | footer(1)
    let input_height = input_area_height(app, area.width);
    let steer_height: u16 = if app.pending_steer.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),               // messages
            Constraint::Length(steer_height), // steer indicator (0 when absent)
            Constraint::Length(input_height), // input area
            Constraint::Length(1),            // footer (stats left, model right)
        ])
        .split(area);

    draw_messages(frame, app, theme, chunks[0]);
    if steer_height > 0 {
        draw_steer_indicator(frame, app, theme, chunks[1]);
    }
    draw_input(frame, app, theme, chunks[2]);
    draw_footer(frame, app, theme, chunks[3]);

    // Session picker overlay
    if app.mode == AppMode::SessionPicker {
        draw_session_picker(frame, app, theme, area);
    }

    // Task picker overlay
    if app.mode == AppMode::TaskPicker {
        if app.task_picker_detail.is_some() {
            draw_task_detail(frame, app, theme, area);
        } else {
            draw_task_picker(frame, app, theme, area);
        }
    }
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

/// Extract the background color from a line, checking line style first then spans.
fn line_bg_color(line: &Line<'_>) -> Option<ratatui::style::Color> {
    // Check the line-level style first
    if let Some(bg) = line.style.bg {
        return Some(bg);
    }
    // Fall back: check the first span's resolved style
    if let Some(span) = line.spans.first()
        && let Some(bg) = span.style.bg
    {
        return Some(bg);
    }
    None
}

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
        // Preserve the original line-level style (carries bg color)
        result.push(Line::from(spans).style(style));
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
            let wrapped = wrap_line(line, w);
            all_lines.extend(wrapped);
        }
    }

    // Unified post-wrap background fill: any line with a bg color gets
    // padded to full width so the background covers the entire row.
    for line in all_lines.iter_mut() {
        if let Some(bg_color) = line_bg_color(line) {
            let visible = line.width();
            let pad = w.saturating_sub(visible);
            if pad > 0 {
                let pad_style = Style::default().bg(bg_color);
                line.spans.push(Span::styled(" ".repeat(pad), pad_style));
            }
        }
    }

    // Add working indicator if streaming (but not for Idle phase —
    // Idle means no active agent; spinner would be misleading).
    if app.mode == AppMode::Streaming && app.phase != AgentPhase::Idle {
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
// Steer indicator (shown above the input box when a steer message is pending)
// ---------------------------------------------------------------------------

fn draw_steer_indicator(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(ref steer_text) = app.pending_steer else {
        return;
    };

    let w = area.width as usize;
    // Take the first line only, truncate to fit
    let first_line = steer_text.lines().next().unwrap_or("");
    let multi_line = steer_text.contains('\n');
    let prefix = " [steer] ";
    let prefix_w = UnicodeWidthStr::width(prefix);
    let avail = w.saturating_sub(prefix_w + 1); // +1 for right margin
    let first_line_w = UnicodeWidthStr::width(first_line);
    let needs_truncate = first_line_w > avail;
    let display = if needs_truncate {
        format!(
            "{}...",
            truncate_to_width(first_line, avail.saturating_sub(3))
        )
    } else if multi_line {
        format!("{}...", first_line)
    } else {
        first_line.to_string()
    };

    let style = theme
        .italic_fg(theme.dim)
        .bg(theme.tool_pending_bg.to_ratatui());
    let prefix_style = theme
        .italic_fg(theme.muted)
        .bg(theme.tool_pending_bg.to_ratatui());

    let display_w = UnicodeWidthStr::width(display.as_str());
    let total_w = prefix_w + display_w;
    let pad = w.saturating_sub(total_w);

    let line = Line::from(vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::styled(display, style),
        Span::styled(
            " ".repeat(pad),
            Style::default().bg(theme.tool_pending_bg.to_ratatui()),
        ),
    ]);

    frame.render_widget(Paragraph::new(line), area);
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

    // Build left side: stats + session ID
    let mut left_parts: Vec<Span<'static>> = Vec::new();

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

    // Subscription usage limits: (5h 50% 16h | 7d 12% 2d | sonnet 6% 1d)
    if let Some(ref usage) = app.subscription_usage
        && let Some(usage_str) = tau_agent::protocol::format_subscription_usage(usage)
    {
        if !left_parts.is_empty() {
            left_parts.push(Span::styled(" ", dim));
        }
        left_parts.push(Span::styled(usage_str, dim));
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

    // Session ID with nav context (after stats)
    if !left_parts.is_empty() {
        left_parts.push(Span::styled(" ", dim));
    }
    // Show breadcrumb: ^parent > current [N]
    if !app.nav_stack.is_empty() || app.parent_id.is_some() {
        if let Some(pid) = &app.parent_id {
            left_parts.push(Span::styled(
                format!("^{} > ", &pid[..pid.len().min(6)]),
                dim,
            ));
        } else if let Some(entry) = app.nav_stack.last() {
            left_parts.push(Span::styled(
                format!("^{} > ", &entry.session_id[..entry.session_id.len().min(6)]),
                dim,
            ));
        }
    }
    left_parts.push(Span::styled(app.session_id.clone(), dim));
    if app.child_count > 0 {
        left_parts.push(Span::styled(format!(" [{}]", app.child_count), dim));
    }
    if let Some((task_id, _title)) = &app.current_task_id {
        left_parts.push(Span::styled(format!(" T#{}", task_id), dim));
    }

    // Build right side: model name + connection status
    let right_text = if app.server_done {
        format!("{}/{} (disconnected)", app.provider, app.model)
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

// ---------------------------------------------------------------------------
// Session Picker overlay -- tree view with state, tagline, context, idle time
// ---------------------------------------------------------------------------

/// Build a flat display list from sessions, ordered as a tree.
/// Returns (display_index, session_index, depth, is_last_sibling) tuples.
fn build_session_tree(sessions: &[tau_agent::protocol::SessionInfo]) -> Vec<(usize, usize, bool)> {
    // Map parent_id -> children indices
    let mut children_of: std::collections::HashMap<Option<&str>, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        children_of
            .entry(s.parent_id.as_deref())
            .or_default()
            .push(i);
    }

    let mut result = Vec::new();
    fn walk_recursive(
        parent: Option<&str>,
        depth: usize,
        sessions: &[tau_agent::protocol::SessionInfo],
        children_of: &std::collections::HashMap<Option<&str>, Vec<usize>>,
        result: &mut Vec<(usize, usize, bool)>,
    ) {
        let Some(children) = children_of.get(&parent) else {
            return;
        };
        let count = children.len();
        for (pos, &idx) in children.iter().enumerate() {
            let is_last = pos + 1 == count;
            result.push((idx, depth, is_last));
            walk_recursive(
                Some(&sessions[idx].id),
                depth + 1,
                sessions,
                children_of,
                result,
            );
        }
    }

    walk_recursive(None, 0, sessions, &children_of, &mut result);

    // If there are orphans (parent_id set but parent not in list), add them at root level
    let in_tree: std::collections::HashSet<usize> = result.iter().map(|&(i, _, _)| i).collect();
    for (i, _s) in sessions.iter().enumerate() {
        if !in_tree.contains(&i) {
            result.push((i, 0, true));
        }
    }

    result
}

/// Format a duration as a compact idle time string.
/// Truncate `s` to at most `max_width` display columns, respecting char
/// boundaries. Unlike `tau_agent::truncate_str` which counts bytes, this
/// accounts for wide characters (CJK, emoji, box-drawing glyphs).
fn truncate_to_width(s: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max_width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

fn format_idle_time(last_activity: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let delta = (now - last_activity).max(0);
    if delta < 60 {
        return String::new(); // too recent, don't show
    }
    if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86400 {
        format!("{}h", delta / 3600)
    } else {
        format!("{}d", delta / 86400)
    }
}

fn draw_session_picker(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    use ratatui::widgets::Clear;

    let filtered = app.picker_filtered_indices();

    // Use most of the screen width
    let picker_width: u16 = (area.width * 3 / 4)
        .max(50)
        .min(area.width.saturating_sub(2));
    // Height: sessions + footer(1) + borders(2), clamped to area
    let content_lines = filtered.len().max(1) as u16;
    let picker_height = (content_lines + 3).min(area.height.saturating_sub(2));

    // Position: centered
    let x = (area.width.saturating_sub(picker_width)) / 2;
    let y = (area.height.saturating_sub(picker_height)) / 2;
    let picker_area = Rect::new(x, y, picker_width, picker_height);

    // Clear the area behind the overlay
    frame.render_widget(Clear, picker_area);

    // Title includes filter and project scope when active
    let title = if !app.picker_filter.is_empty() {
        format!(" Sessions [/{}] ", app.picker_filter)
    } else if app.picker_filter_mode {
        " Sessions [/] ".to_string()
    } else if let Some(ref pf) = app.picker_project_filter {
        if app.picker_show_all_projects {
            " Sessions [all] ".to_string()
        } else {
            format!(" Sessions [{}] ", pf)
        }
    } else {
        " Sessions ".to_string()
    };

    // Border
    let border_style = Style::default().fg(theme.accent.to_ratatui());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent.to_ratatui())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
    let inner = block.inner(picker_area);
    frame.render_widget(block, picker_area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let w = inner.width as usize;

    // Build a set of filtered indices for quick lookup and tree building
    let filtered_set: std::collections::HashSet<usize> = filtered.iter().copied().collect();

    if app.picker_sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            " (loading...)",
            theme.fg(theme.muted),
        )));
    } else if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no matches)",
            theme.fg(theme.muted),
        )));
    } else {
        let tree = build_session_tree(&app.picker_sessions);

        // Map from session_idx -> position in filtered list (for cursor tracking)
        let mut filtered_pos: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (pos, &idx) in filtered.iter().enumerate() {
            filtered_pos.insert(idx, pos);
        }

        for &(session_idx, depth, is_last) in &tree {
            // Skip sessions not in the filtered set
            if !filtered_set.contains(&session_idx) {
                continue;
            }

            let session = &app.picker_sessions[session_idx];
            let is_current = session.id == app.session_id;
            let cursor_pos = filtered_pos.get(&session_idx).copied();
            let is_selected = cursor_pos == Some(app.picker_cursor);
            let is_confirming_delete = cursor_pos == app.picker_confirm_delete;
            let is_confirming_archive = cursor_pos == app.picker_confirm_archive;
            let is_editing_tagline = app
                .picker_edit_tagline
                .as_ref()
                .is_some_and(|(c, _, _)| cursor_pos == Some(*c));

            // Delete confirmation
            if is_confirming_delete {
                let id_short = &session.id[..session.id.len().min(8)];
                let confirm_text = format!(" Delete {}? y/n", id_short);
                let confirm_padded = if confirm_text.len() < w {
                    format!("{}{}", confirm_text, " ".repeat(w - confirm_text.len()))
                } else {
                    confirm_text[..w].to_string()
                };
                let style = Style::default()
                    .fg(theme.error.to_ratatui())
                    .bg(ThemeColor::Rgb(0x3c, 0x28, 0x28).to_ratatui());
                lines.push(Line::from(Span::styled(confirm_padded, style)));
                continue;
            }

            // Archive confirmation
            if is_confirming_archive {
                let id_short = &session.id[..session.id.len().min(8)];
                let confirm_text = format!(" Archive {}? y/n", id_short);
                let confirm_padded = if confirm_text.len() < w {
                    format!("{}{}", confirm_text, " ".repeat(w - confirm_text.len()))
                } else {
                    confirm_text[..w].to_string()
                };
                let style = Style::default()
                    .fg(theme.muted.to_ratatui())
                    .bg(ThemeColor::Rgb(0x28, 0x28, 0x3c).to_ratatui());
                lines.push(Line::from(Span::styled(confirm_padded, style)));
                continue;
            }

            // Tagline editing
            if is_editing_tagline
                && let Some((_, ref edit_text, text_cursor)) = app.picker_edit_tagline
            {
                let id_short = &session.id[..session.id.len().min(8)];
                let prefix = format!(" {} tagline: ", id_short);
                let base_style = Style::default()
                    .fg(theme.accent.to_ratatui())
                    .bg(theme.selected_bg.to_ratatui());
                let cursor_style = base_style.add_modifier(ratatui::style::Modifier::REVERSED);

                // Character at cursor position (or space if at end of text)
                let cursor_char = if text_cursor < edit_text.len() {
                    edit_text[text_cursor..]
                        .chars()
                        .next()
                        .expect("text_cursor < len guarantees a char")
                        .to_string()
                } else {
                    " ".to_string()
                };
                let after_byte_pos = text_cursor + cursor_char.len();
                let before_cursor = &edit_text[..text_cursor];
                let after_cursor = if after_byte_pos <= edit_text.len() {
                    &edit_text[after_byte_pos..]
                } else {
                    ""
                };

                // Calculate total display width for padding
                let prefix_w = UnicodeWidthStr::width(prefix.as_str());
                let before_w = UnicodeWidthStr::width(before_cursor);
                let cursor_w = UnicodeWidthStr::width(cursor_char.as_str()).max(1);
                let after_w = UnicodeWidthStr::width(after_cursor);
                let total_w = prefix_w + before_w + cursor_w + after_w;

                let padding = if total_w < w {
                    " ".repeat(w - total_w)
                } else {
                    String::new()
                };

                // Build spans: prefix + before + cursor_char (reversed) + after + padding
                let edit_spans = vec![
                    Span::styled(format!("{}{}", prefix, before_cursor), base_style),
                    Span::styled(cursor_char, cursor_style),
                    Span::styled(format!("{}{}", after_cursor, padding), base_style),
                ];
                lines.push(Line::from(edit_spans));
                continue;
            }

            // Build spans for this row
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0usize;

            // Tree indent with connectors (flatten when filtering).
            // Note: box-drawing glyphs (└ ├ │ ─) are 3 bytes in UTF-8 but
            // render at width 1, so we must use display width (not byte
            // length) when tracking column position.
            let connector = if !app.picker_filter.is_empty() || depth == 0 {
                String::new()
            } else if is_last {
                format!("{}└── ", "│   ".repeat(depth - 1))
            } else {
                format!("{}├── ", "│   ".repeat(depth - 1))
            };
            spans.push(Span::raw(format!(" {}", connector)));
            used += 1 + UnicodeWidthStr::width(connector.as_str());

            // Fold indicator: ▼ (has children, unfolded), ▶ (has children,
            // folded), or two spaces (no children) for alignment.
            let fold_indicator = if session.child_count > 0 {
                if app.picker_folded.contains(&session.id) {
                    "▶ "
                } else {
                    "▼ "
                }
            } else {
                "  "
            };
            spans.push(Span::styled(
                fold_indicator.to_string(),
                theme.fg(theme.dim),
            ));
            // Both triangle glyphs render at width 1, plus one trailing space.
            used += 2;

            // State indicator: ● (idle/green), ▶ (active), * (current)
            let (state_char, state_color) = if is_current {
                ("*", theme.accent)
            } else if session.state == "idle" {
                ("●", theme.success)
            } else {
                // Active states: thinking, responding, tool_exec, etc.
                ("▶", theme.warning)
            };
            spans.push(Span::styled(
                format!("{} ", state_char),
                theme.fg(state_color),
            ));
            used += 2;

            // Session ID
            let id_len = 8.min(session.id.len());
            let id_short = &session.id[..id_len];
            let id_style = if is_current || is_selected {
                theme.fg(theme.text)
            } else {
                theme.fg(theme.muted)
            };
            spans.push(Span::styled(id_short.to_string(), id_style));
            used += id_len;

            // Project badge: when showing all projects, display [project] after session ID.
            if app.picker_show_all_projects || app.picker_project_filter.is_none() {
                if let Some(ref pn) = session.project_name {
                    let badge = format!(" [{}]", pn);
                    let badge_w = UnicodeWidthStr::width(badge.as_str());
                    spans.push(Span::styled(badge, theme.fg(theme.dim)));
                    used += badge_w;
                }
            }

            // Right-side info columns: fixed-width, right-aligned, so the
            // same field always appears in the same column across rows.
            //
            // Column widths (chars), tuned for typical values:
            //   ctx%:  4   (e.g. "100%")
            //   cost:  6   (e.g. "$12.34")
            //   idle:  3   (e.g. "59m" / "23h" / "99d")
            //   msgs:  5   (e.g. "#1234"; "#" disambiguates from idle "m")
            const CTX_W: usize = 4;
            const COST_W: usize = 6;
            const IDLE_W: usize = 3;
            const MSGS_W: usize = 5;
            // Two-space gap before the block, plus two spaces between columns.
            const COL_GAP: usize = 2;

            let ctx_str = session
                .context_pct
                .map(|p| format!("{:.0}%", p))
                .unwrap_or_default();
            let idle_str = format_idle_time(session.last_activity);
            // Use "#" prefix for message count to disambiguate from idle
            // minutes ("m"). Both used to render as "...m" on the same row.
            let msg_str = format!("#{}", session.message_count);
            let cost_str = if session.stats.cost > 0.0 {
                format!("${:.2}", session.stats.cost)
            } else {
                String::new()
            };

            // Right-align each value within its fixed-width column. Missing
            // values render as all-spaces of the same width so columns line up.
            let right_text = format!(
                "  {:>ctx_w$}  {:>cost_w$}  {:>idle_w$}  {:>msgs_w$}",
                ctx_str,
                cost_str,
                idle_str,
                msg_str,
                ctx_w = CTX_W,
                cost_w = COST_W,
                idle_w = IDLE_W,
                msgs_w = MSGS_W,
            );
            // Total width is deterministic: leading 2-space gap + 4 columns
            // separated by 2 spaces each.
            let right_len =
                COL_GAP + CTX_W + COL_GAP + COST_W + COL_GAP + IDLE_W + COL_GAP + MSGS_W;
            debug_assert_eq!(right_text.len(), right_len);

            // Tagline fills the middle. Use display width (not byte length)
            // so wide chars (CJK, emoji) don't break alignment.
            let tagline_space = w.saturating_sub(used + 1 + right_len + 1);
            let tagline_display = if tagline_space >= 4 {
                if let Some(ref tl) = session.tagline {
                    if UnicodeWidthStr::width(tl.as_str()) > tagline_space {
                        format!(
                            " {}...",
                            truncate_to_width(tl, tagline_space.saturating_sub(3))
                        )
                    } else {
                        format!(" {}", tl)
                    }
                } else {
                    // Fall back to model name
                    let m = &session.model;
                    if UnicodeWidthStr::width(m.as_str()) + 1 > tagline_space {
                        format!(
                            " {}...",
                            truncate_to_width(m, tagline_space.saturating_sub(3))
                        )
                    } else {
                        format!(" {}", m)
                    }
                }
            } else {
                String::new()
            };

            spans.push(Span::styled(tagline_display, theme.italic_fg(theme.dim)));

            // Anchor the right block to the right edge: compute padding from
            // the actual rendered width of all spans so far, not from the
            // running `used` counter. This makes the right column robust
            // against any residual width-tracking bugs upstream.
            let line_so_far_width: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = w.saturating_sub(line_so_far_width + right_len);
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }

            spans.push(Span::styled(right_text, theme.fg(theme.dim)));

            // Row style (selection background)
            let row_style = if is_selected {
                Style::default().bg(theme.selected_bg.to_ratatui())
            } else {
                Style::default()
            };

            let mut line = Line::from(spans);
            // Pad the entire line to full width for selection highlight
            let line_w = line.width();
            if line_w < w {
                line.spans
                    .push(Span::styled(" ".repeat(w - line_w), row_style));
            }
            line = line.style(row_style);

            lines.push(line);
        }
    }

    // Footer hint
    let hint = if app.picker_filter_mode {
        " type to filter  enter accept  esc clear"
    } else if app.picker_edit_tagline.is_some() {
        " type tagline  ←/→ move  enter save  esc cancel"
    } else {
        " /search  j/k nav  enter switch  f fold  r rename  P project  A archive  R restore  D del  tab/esc close"
    };
    let hint_display: String = if hint.len() > w {
        hint[..w].to_string()
    } else {
        hint.to_string()
    };
    lines.push(Line::from(Span::styled(hint_display, theme.fg(theme.dim))));

    // Scroll the session list if needed
    let available_lines = inner.height as usize;
    let session_lines = available_lines.saturating_sub(1); // reserve 1 for hint

    if lines.len() > available_lines {
        let num_sessions = lines.len() - 1; // exclude hint
        let hint_line = lines.pop().expect("lines not empty: hint was just pushed");

        let scroll_start = if app.picker_cursor >= session_lines {
            app.picker_cursor - session_lines + 1
        } else {
            0
        };
        let scroll_end = (scroll_start + session_lines).min(num_sessions);

        let mut visible: Vec<Line<'static>> = lines[scroll_start..scroll_end].to_vec();
        visible.push(hint_line);
        lines = visible;
    }

    let text = Text::from(lines);
    frame.render_widget(Paragraph::new(text), inner);
}

// ---------------------------------------------------------------------------
// Task Picker overlay -- tree view with state, priority, session
// ---------------------------------------------------------------------------

/// Return the display icon and style for a task state string.
fn task_state_style(state: &str, theme: &Theme) -> (&'static str, Style) {
    match state {
        "interactive" => ("◆", theme.fg(theme.accent)),
        "planning" | "refining" => ("○", theme.fg(theme.dim)),
        "ready" => ("●", theme.fg(theme.muted)),
        "active" => ("▶", theme.fg(theme.success)),
        "review" => ("◎", theme.fg(theme.warning)),
        "approved" => ("✓", theme.bold_fg(theme.success)),
        "merging" => ("⟳", theme.fg(theme.accent)),
        "merged" => ("✔", theme.fg(theme.dim)),
        "failed" => ("✗", theme.fg(theme.error)),
        "closed" => ("✕", theme.fg(theme.dim)),
        _ => ("?", theme.fg(theme.dim)),
    }
}

/// Determine `is_last` sibling for each item in a pre-ordered depth list.
///
/// A task at depth D is the last sibling if no later filtered task shares
/// the same depth without an intervening task at a shallower depth.
fn compute_is_last_flags(
    tasks: &[(usize, tau_agent::protocol::TaskInfo)],
    filtered: &[usize],
) -> Vec<bool> {
    filtered
        .iter()
        .enumerate()
        .map(|(pos, &idx)| {
            let (depth, _) = &tasks[idx];
            for &next_idx in &filtered[pos + 1..] {
                let (next_depth, _) = &tasks[next_idx];
                if *next_depth <= *depth {
                    // Same or shallower depth → this is last only if shallower
                    return *next_depth < *depth;
                }
            }
            // No sibling found → this is the last
            true
        })
        .collect()
}

fn draw_task_picker(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    use ratatui::widgets::Clear;

    let filtered = app.task_picker_filtered_indices();

    // Use most of the screen width
    let picker_width: u16 = (area.width * 3 / 4)
        .max(50)
        .min(area.width.saturating_sub(2));
    // Height: tasks + footer(1) + borders(2), clamped to area
    let content_lines = filtered.len().max(1) as u16;
    let picker_height = (content_lines + 3).min(area.height.saturating_sub(2));

    // Position: centered
    let x = (area.width.saturating_sub(picker_width)) / 2;
    let y = (area.height.saturating_sub(picker_height)) / 2;
    let picker_area = Rect::new(x, y, picker_width, picker_height);

    // Clear the area behind the overlay
    frame.render_widget(Clear, picker_area);

    // Title includes filter/create mode when active
    let title = if app.task_picker_create_mode && !app.task_picker_filter.is_empty() {
        format!(" Tasks [+{}] ", app.task_picker_filter)
    } else if app.task_picker_create_mode {
        " Tasks [+] ".to_string()
    } else if !app.task_picker_filter.is_empty() {
        format!(" Tasks [/{}] ", app.task_picker_filter)
    } else if app.task_picker_filter_mode {
        " Tasks [/] ".to_string()
    } else {
        " Tasks ".to_string()
    };

    // Border
    let border_style = Style::default().fg(theme.accent.to_ratatui());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent.to_ratatui())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
    let inner = block.inner(picker_area);
    frame.render_widget(block, picker_area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let w = inner.width as usize;

    // Right-side column widths
    const PRI_W: usize = 3; // "p0" .. "p9"
    const SES_W: usize = 8; // session id
    const COL_GAP: usize = 2;
    let right_len = COL_GAP + PRI_W + COL_GAP + SES_W;

    if app.picker_tasks.is_empty() {
        lines.push(Line::from(Span::styled(
            " (loading...)",
            theme.fg(theme.muted),
        )));
    } else if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no matches)",
            theme.fg(theme.muted),
        )));
    } else {
        // Compute is_last flags for tree connectors
        let is_last_flags = compute_is_last_flags(&app.picker_tasks, &filtered);

        for (filter_pos, &task_idx) in filtered.iter().enumerate() {
            let (depth, ref task) = app.picker_tasks[task_idx];
            let is_selected = filter_pos == app.task_picker_cursor;
            let is_last = is_last_flags[filter_pos];

            // Confirmation prompt replaces the selected row
            if is_selected {
                if let Some((confirm_cursor, ref confirm_label, _)) = app.task_picker_confirm {
                    if confirm_cursor == app.task_picker_cursor {
                        let confirm_text = format!(" {} y/n", confirm_label);
                        let confirm_padded = if confirm_text.len() < w {
                            format!("{}{}", confirm_text, " ".repeat(w - confirm_text.len()))
                        } else {
                            confirm_text[..w].to_string()
                        };
                        let style = Style::default()
                            .fg(theme.accent.to_ratatui())
                            .bg(theme.selected_bg.to_ratatui());
                        lines.push(Line::from(Span::styled(confirm_padded, style)));
                        continue;
                    }
                }
            }

            // Build spans for this row
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0usize;

            // Tree indent with connectors (flatten when filtering)
            let connector = if !app.task_picker_filter.is_empty() || depth == 0 {
                String::new()
            } else if is_last {
                format!("{}└── ", "│   ".repeat(depth - 1))
            } else {
                format!("{}├── ", "│   ".repeat(depth - 1))
            };
            spans.push(Span::styled(format!(" {}", connector), theme.fg(theme.dim)));
            used += 1 + UnicodeWidthStr::width(connector.as_str());

            // State icon
            let (icon, icon_style) = task_state_style(&task.state, theme);
            spans.push(Span::styled(format!("{} ", icon), icon_style));
            // State icons are typically width 1 + 1 space = 2
            used += UnicodeWidthStr::width(icon).max(1) + 1;

            // Task ID (right-aligned, 4 chars + 1 space)
            let id_style = if is_selected {
                theme.fg(theme.text)
            } else {
                theme.fg(theme.muted)
            };
            spans.push(Span::styled(format!("{:>4} ", task.id), id_style));
            used += 5;

            // Right-side info: priority + session
            let pri_str = format!("p{}", task.priority);
            let session_str: String = task
                .session_id
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(SES_W)
                .collect();

            let right_text = format!(
                "  {:>pri_w$}  {:>ses_w$}",
                pri_str,
                session_str,
                pri_w = PRI_W,
                ses_w = SES_W,
            );

            // Title fills the middle
            let title_space = w.saturating_sub(used + right_len + 1);
            let title_style = if task.state == "merged" || task.state == "closed" {
                theme.italic_fg(theme.dim)
            } else if is_selected {
                theme.fg(theme.text)
            } else {
                Style::default()
            };

            let title_display = if title_space >= 4 {
                if UnicodeWidthStr::width(task.title.as_str()) > title_space {
                    format!(
                        " {}...",
                        truncate_to_width(&task.title, title_space.saturating_sub(4))
                    )
                } else {
                    format!(" {}", task.title)
                }
            } else {
                String::new()
            };

            spans.push(Span::styled(title_display, title_style));

            // Pad to push right-side block to the right edge
            let line_so_far_width: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = w.saturating_sub(line_so_far_width + right_len);
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }

            spans.push(Span::styled(right_text, theme.fg(theme.dim)));

            // Row style (selection background)
            let row_style = if is_selected {
                Style::default().bg(theme.selected_bg.to_ratatui())
            } else {
                Style::default()
            };

            let mut line = Line::from(spans);
            // Pad the entire line to full width for selection highlight
            let line_w = line.width();
            if line_w < w {
                line.spans
                    .push(Span::styled(" ".repeat(w - line_w), row_style));
            }
            line = line.style(row_style);

            lines.push(line);
        }
    }

    // Footer hint
    let hint = if app.task_picker_confirm.is_some() {
        " y/enter confirm  any key cancel"
    } else if app.task_picker_create_mode {
        " type title  enter create  esc cancel"
    } else if app.task_picker_filter_mode {
        " type to filter  enter accept  esc clear"
    } else {
        " /search  c new  j/k nav  enter view  a approve  r ready  d dispatch  g goto  F2/esc close"
    };
    let hint_display: String = if hint.len() > w {
        hint[..w].to_string()
    } else {
        hint.to_string()
    };
    lines.push(Line::from(Span::styled(hint_display, theme.fg(theme.dim))));

    // Scroll the task list if needed
    let available_lines = inner.height as usize;
    let task_lines = available_lines.saturating_sub(1); // reserve 1 for hint

    if lines.len() > available_lines {
        let num_tasks = lines.len() - 1; // exclude hint
        let hint_line = lines.pop().expect("lines not empty: hint was just pushed");

        let scroll_start = if app.task_picker_cursor >= task_lines {
            app.task_picker_cursor - task_lines + 1
        } else {
            0
        };
        let scroll_end = (scroll_start + task_lines).min(num_tasks);

        let mut visible: Vec<Line<'static>> = lines[scroll_start..scroll_end].to_vec();
        visible.push(hint_line);
        lines = visible;
    }

    let text = Text::from(lines);
    frame.render_widget(Paragraph::new(text), inner);
}

fn draw_task_detail(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    use ratatui::widgets::Clear;

    let detail = match app.task_picker_detail {
        Some(ref d) => d,
        None => return,
    };

    let task = &detail.task;

    // Use most of the screen width
    let picker_width: u16 = (area.width * 3 / 4)
        .max(50)
        .min(area.width.saturating_sub(2));
    let picker_height = area.height.saturating_sub(4).max(10);

    // Position: centered
    let x = (area.width.saturating_sub(picker_width)) / 2;
    let y = (area.height.saturating_sub(picker_height)) / 2;
    let picker_area = Rect::new(x, y, picker_width, picker_height);

    // Clear the area behind the overlay
    frame.render_widget(Clear, picker_area);

    // Title
    let title_text = format!(" Task #{}: {} ", task.id, task.title);
    let title_truncated = if UnicodeWidthStr::width(title_text.as_str()) > picker_width as usize - 2
    {
        format!(
            "{}... ",
            truncate_to_width(&title_text, (picker_width as usize).saturating_sub(6))
        )
    } else {
        title_text
    };

    let border_style = Style::default().fg(theme.accent.to_ratatui());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Span::styled(
            title_truncated,
            Style::default()
                .fg(theme.accent.to_ratatui())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
    let inner = block.inner(picker_area);
    frame.render_widget(block, picker_area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let w = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // State / priority / skip review
    let (icon, icon_style) = task_state_style(&task.state, theme);
    let skip = if task.skip_review { "yes" } else { "no" };
    lines.push(Line::from(vec![
        Span::styled("  State: ", theme.bold_fg(theme.text)),
        Span::styled(format!("{} ", icon), icon_style),
        Span::styled(format!("{}  ", task.state), theme.fg(theme.text)),
        Span::styled("Priority: ", theme.bold_fg(theme.text)),
        Span::styled(format!("{}  ", task.priority), theme.fg(theme.text)),
        Span::styled("Skip review: ", theme.bold_fg(theme.text)),
        Span::styled(skip.to_string(), theme.fg(theme.text)),
    ]));

    // Branch / session / parent
    let branch = task.branch.as_deref().unwrap_or("none");
    let session = task.session_id.as_deref().unwrap_or("none");
    let parent = task
        .parent_id
        .map(|p| format!("#{}", p))
        .unwrap_or_else(|| "none".to_string());
    lines.push(Line::from(vec![
        Span::styled("  Branch: ", theme.bold_fg(theme.text)),
        Span::styled(format!("{}  ", branch), theme.fg(theme.text)),
        Span::styled("Session: ", theme.bold_fg(theme.text)),
        Span::styled(format!("{}  ", session), theme.fg(theme.text)),
        Span::styled("Parent: ", theme.bold_fg(theme.text)),
        Span::styled(parent, theme.fg(theme.text)),
    ]));

    // Tags
    let tags_str = task
        .tags
        .as_ref()
        .and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        })
        .unwrap_or_else(|| "-".to_string());
    lines.push(Line::from(vec![
        Span::styled("  Tags: ", theme.bold_fg(theme.text)),
        Span::styled(tags_str, theme.fg(theme.text)),
    ]));

    // Blank separator
    lines.push(Line::from(""));

    // Confirmation prompt (if any)
    if let Some((_, ref confirm_label, _)) = app.task_picker_confirm {
        lines.push(Line::from(Span::styled(
            format!("  {} y/n", confirm_label),
            Style::default()
                .fg(theme.accent.to_ratatui())
                .add_modifier(ratatui::style::Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    // Messages
    if !detail.messages.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  Messages ({})", detail.messages.len()),
            theme.bold_fg(theme.text),
        )));
        for msg in &detail.messages {
            let author = msg.author.as_deref().unwrap_or("unknown");
            let max_preview = w.saturating_sub(20);
            let first_line = msg.content.lines().next().unwrap_or("");
            let truncated = first_line.chars().count() > max_preview;
            let multiline = msg.content.contains('\n');
            let preview: String = first_line.chars().take(max_preview).collect();
            let ellipsis = if truncated || multiline { "..." } else { "" };
            lines.push(Line::from(vec![
                Span::styled(format!("    #{} ", msg.id), theme.fg(theme.muted)),
                Span::styled(format!("[{}] ", author), theme.fg(theme.dim)),
                Span::styled(format!("{}{}", preview, ellipsis), theme.fg(theme.text)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  Messages: (none)",
            theme.fg(theme.muted),
        )));
    }

    lines.push(Line::from(""));

    // Subtasks
    if !detail.subtasks.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  Subtasks ({})", detail.subtasks.len()),
            theme.bold_fg(theme.text),
        )));
        for st in &detail.subtasks {
            let (st_icon, st_style) = task_state_style(&st.state, theme);
            lines.push(Line::from(vec![
                Span::styled(format!("    #{:<4} ", st.id), theme.fg(theme.muted)),
                Span::styled(format!("{} ", st_icon), st_style),
                Span::styled(st.title.clone(), theme.fg(theme.text)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  Subtasks: (none)",
            theme.fg(theme.muted),
        )));
    }

    // Relations
    if !detail.relations.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  Relations ({})", detail.relations.len()),
            theme.bold_fg(theme.text),
        )));
        for rel in &detail.relations {
            lines.push(Line::from(Span::styled(
                format!("    #{} {} #{}", rel.from_task, rel.relation, rel.to_task),
                theme.fg(theme.text),
            )));
        }
    }

    // Footer hint
    let hint = if app.task_picker_confirm.is_some() {
        " y/enter confirm  any key cancel"
    } else {
        " a approve  r ready  d dispatch  g goto session  j/k scroll  esc back"
    };
    let hint_display: String = if hint.len() > w {
        hint[..w].to_string()
    } else {
        hint.to_string()
    };

    // Scroll handling
    let available_lines = inner.height as usize;
    let content_lines = available_lines.saturating_sub(1); // reserve 1 for hint

    let hint_line = Line::from(Span::styled(hint_display, theme.fg(theme.dim)));

    if lines.len() > content_lines {
        let scroll = detail.scroll.min(lines.len().saturating_sub(content_lines));
        let end = (scroll + content_lines).min(lines.len());
        let mut visible: Vec<Line<'static>> = lines[scroll..end].to_vec();
        visible.push(hint_line);
        lines = visible;
    } else {
        lines.push(hint_line);
    }

    let text = Text::from(lines);
    frame.render_widget(Paragraph::new(text), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::dark;

    #[test]
    fn truncate_to_width_ascii() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("", 10), "");
    }

    #[test]
    fn truncate_to_width_wide_chars() {
        // CJK chars are width 2.
        assert_eq!(truncate_to_width("日本語", 4), "日本");
        // Width 3 is not enough for two CJK chars (would be 4) → just one.
        assert_eq!(truncate_to_width("日本語", 3), "日");
        // Width 0 → empty.
        assert_eq!(truncate_to_width("日本", 0), "");
    }

    #[test]
    fn truncate_to_width_box_drawing() {
        // Box-drawing chars are 3 bytes UTF-8 but width 1.
        let s = "│   ├── ";
        assert_eq!(UnicodeWidthStr::width(s), 8);
        assert!(s.len() > 8); // bytes > width
        assert_eq!(truncate_to_width(s, 4), "│   ");
    }

    /// Regression test for the bug where connector display width was
    /// computed as byte length, causing right-side columns to drift left
    /// at deeper tree depths.
    #[test]
    fn connector_width_matches_display_not_bytes() {
        for depth in 1..=5usize {
            let last = format!("{}└── ", "│   ".repeat(depth - 1));
            let mid = format!("{}├── ", "│   ".repeat(depth - 1));
            // Each level adds 4 columns; the leaf glyph adds 4 columns.
            let expected_width = depth * 4;
            assert_eq!(
                UnicodeWidthStr::width(last.as_str()),
                expected_width,
                "└── connector at depth {depth}"
            );
            assert_eq!(
                UnicodeWidthStr::width(mid.as_str()),
                expected_width,
                "├── connector at depth {depth}"
            );
            // Byte length is strictly larger than display width because of
            // the 3-byte box-drawing glyphs.
            assert!(last.len() > expected_width);
            assert!(mid.len() > expected_width);
        }
    }

    /// Simulate the picker layout math to verify the right-side block
    /// lands at the same column regardless of tree depth. This mirrors the
    /// logic in `draw_session_picker`.
    #[test]
    fn picker_right_block_aligned_across_depths() {
        const W: usize = 80;
        const RIGHT_LEN: usize = 25; // matches the actual right_text width

        // For each depth, compute where the right block starts.
        let mut right_starts = Vec::new();
        for depth in 0..=4usize {
            let connector = if depth == 0 {
                String::new()
            } else {
                format!("{}├── ", "│   ".repeat(depth - 1))
            };

            // Mimic the row builder.
            let mut spans: Vec<String> = Vec::new();
            spans.push(format!(" {}", connector)); // leading space + connector
            spans.push("▼ ".to_string()); // fold indicator
            spans.push("* ".to_string()); // state
            spans.push("s123abcd".to_string()); // 8-char id

            // tagline (kept short so it fits everywhere)
            let tagline = " example tagline";
            spans.push(tagline.to_string());

            let line_so_far_width: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.as_str()))
                .sum();
            let pad = W.saturating_sub(line_so_far_width + RIGHT_LEN);
            let right_col = line_so_far_width + pad;
            right_starts.push(right_col);
        }

        // All right-column starts should equal W - RIGHT_LEN.
        for (depth, &col) in right_starts.iter().enumerate() {
            assert_eq!(
                col,
                W - RIGHT_LEN,
                "right block start at depth {depth} should be {} but got {}",
                W - RIGHT_LEN,
                col
            );
        }
    }

    #[test]
    fn task_state_icons_cover_all_states() {
        let theme = dark();
        let states = [
            "interactive",
            "planning",
            "refining",
            "ready",
            "active",
            "review",
            "approved",
            "merging",
            "merged",
            "failed",
            "closed",
        ];
        for state in states {
            let (icon, _style) = task_state_style(state, &theme);
            assert_ne!(icon, "?", "state '{}' should have a specific icon", state);
            assert!(
                UnicodeWidthStr::width(icon) >= 1,
                "icon for '{}' should have positive width",
                state
            );
        }
        // Unknown states get "?"
        let (icon, _) = task_state_style("unknown_state", &theme);
        assert_eq!(icon, "?");
    }

    #[test]
    fn task_picker_right_block_aligned_across_depths() {
        // Mirrors the session picker alignment test but for task picker layout.
        const W: usize = 80;
        const PRI_W: usize = 3;
        const SES_W: usize = 8;
        const COL_GAP: usize = 2;
        const RIGHT_LEN: usize = COL_GAP + PRI_W + COL_GAP + SES_W;

        let mut right_starts = Vec::new();
        for depth in 0..=4usize {
            let connector = if depth == 0 {
                String::new()
            } else {
                format!("{}├── ", "│   ".repeat(depth - 1))
            };

            let mut spans: Vec<String> = Vec::new();
            spans.push(format!(" {}", connector)); // leading space + connector
            spans.push("◆ ".to_string()); // state icon
            spans.push(format!("{:>4} ", 14)); // task ID

            let title = " Example task title";
            spans.push(title.to_string());

            let line_so_far_width: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.as_str()))
                .sum();
            let pad = W.saturating_sub(line_so_far_width + RIGHT_LEN);
            let right_col = line_so_far_width + pad;
            right_starts.push(right_col);
        }

        for (depth, &col) in right_starts.iter().enumerate() {
            assert_eq!(
                col,
                W - RIGHT_LEN,
                "task picker right block start at depth {depth} should be {} but got {}",
                W - RIGHT_LEN,
                col
            );
        }
    }

    #[test]
    fn compute_is_last_flags_simple_tree() {
        use tau_agent::protocol::TaskInfo;

        let make_task = |id: i64| TaskInfo {
            id,
            project_name: String::new(),
            title: format!("Task {}", id),
            state: "active".to_string(),
            priority: 0,
            parent_id: None,
            tags: None,
            affected_files: None,
            branch: None,
            worktree_path: None,
            session_id: None,
            skip_review: false,
            skip_planning: false,
            require_approval: false,
            sandbox_profile: None,
            created_at: 0,
            updated_at: 0,
        };

        // Tree structure:
        // depth 0: task 1
        //   depth 1: task 2
        //   depth 1: task 3
        // depth 0: task 4
        let tasks: Vec<(usize, TaskInfo)> = vec![
            (0, make_task(1)),
            (1, make_task(2)),
            (1, make_task(3)),
            (0, make_task(4)),
        ];
        let filtered: Vec<usize> = vec![0, 1, 2, 3];
        let flags = compute_is_last_flags(&tasks, &filtered);
        // task 1 (depth 0): not last (task 4 follows at depth 0)
        assert!(!flags[0], "task 1 should not be last sibling");
        // task 2 (depth 1): not last (task 3 follows at depth 1)
        assert!(!flags[1], "task 2 should not be last sibling");
        // task 3 (depth 1): last (task 4 follows at depth 0 < 1)
        assert!(flags[2], "task 3 should be last sibling");
        // task 4 (depth 0): last (no more tasks)
        assert!(flags[3], "task 4 should be last sibling");
    }
}
