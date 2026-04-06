//! Layout and rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use tau::protocol::format_tokens;
use tau::truncate_str;
use tau::types::AgentPhase;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppMode};
use crate::theme::{Theme, ThemeColor};

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

    // Session picker overlay
    if app.mode == AppMode::SessionPicker {
        draw_session_picker(frame, app, theme, area);
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
        && let Some(usage_str) = tau::protocol::format_subscription_usage(usage)
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
fn build_session_tree(sessions: &[tau::protocol::SessionInfo]) -> Vec<(usize, usize, bool)> {
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
        sessions: &[tau::protocol::SessionInfo],
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

    // Title includes filter when active
    let title = if !app.picker_filter.is_empty() {
        format!(" Sessions [/{}] ", app.picker_filter)
    } else if app.picker_filter_mode {
        " Sessions [/] ".to_string()
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
                    edit_text[text_cursor..].chars().next().unwrap().to_string()
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

            // Tree indent with connectors (flatten when filtering)
            let connector = if !app.picker_filter.is_empty() || depth == 0 {
                String::new()
            } else if is_last {
                format!("{}└── ", "│   ".repeat(depth - 1))
            } else {
                format!("{}├── ", "│   ".repeat(depth - 1))
            };
            spans.push(Span::raw(format!(" {}", connector)));
            used += 1 + connector.len();

            // State indicator
            let (state_char, state_color) = if is_current {
                ("*", theme.accent)
            } else if session.state == "idle" {
                (".", theme.dim)
            } else {
                // Active states: thinking, responding, tool_exec, etc.
                (">", theme.success)
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

            // Right-side info: context%, idle, msgs -- build from right
            let ctx_str = session
                .context_pct
                .map(|p| format!("{:.0}%", p))
                .unwrap_or_default();
            let idle_str = format_idle_time(session.last_activity);
            let msg_str = format!("{}m", session.message_count);

            // Cost string
            let cost_str = if session.stats.cost > 0.0 {
                format!("${:.2}", session.stats.cost)
            } else {
                String::new()
            };

            // Compose right part: "  42%  $0.42  3m  12m"
            let mut right_parts: Vec<String> = Vec::new();
            if !ctx_str.is_empty() {
                right_parts.push(ctx_str);
            }
            if !cost_str.is_empty() {
                right_parts.push(cost_str);
            }
            if !idle_str.is_empty() {
                right_parts.push(idle_str);
            }
            right_parts.push(msg_str);
            let right_text = format!("  {}", right_parts.join("  "));
            let right_len = right_text.len();

            // Tagline fills the middle
            let tagline_space = w.saturating_sub(used + 1 + right_len + 1);
            let tagline_display = if tagline_space >= 4 {
                if let Some(ref tl) = session.tagline {
                    if tl.len() > tagline_space {
                        format!(" {}...", truncate_str(tl, tagline_space.saturating_sub(3)))
                    } else {
                        format!(" {}", tl)
                    }
                } else {
                    // Fall back to model name
                    let m = &session.model;
                    if m.len() + 1 > tagline_space {
                        format!(" {}...", truncate_str(m, tagline_space.saturating_sub(3)))
                    } else {
                        format!(" {}", m)
                    }
                }
            } else {
                String::new()
            };
            let tagline_actual_len = tagline_display.len();

            spans.push(Span::styled(tagline_display, theme.italic_fg(theme.dim)));
            used += tagline_actual_len;

            // Pad between tagline and right info
            let pad = w.saturating_sub(used + right_len);
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
        " /search  j/k nav  enter switch  r rename  A archive  R restore  D del  tab/esc close"
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
