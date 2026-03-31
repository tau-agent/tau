//! Tool rendering system.
//!
//! Each tool can have a custom renderer that controls how it appears in the TUI.
//! Unknown tools use DefaultRenderer.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::HashMap;

use crate::theme::Theme;

/// Maximum output lines shown before clamping.
const DEFAULT_MAX_LINES: usize = 10;

/// Clamp output lines: show first `max` lines, then "... N more lines".
fn clamp_lines<'a>(lines: &[Line<'a>], max: usize, theme: &Theme) -> Vec<Line<'a>> {
    if lines.len() <= max {
        return lines.to_vec();
    }
    let hidden = lines.len() - max;
    let mut result: Vec<Line> = lines[..max].to_vec();
    result.push(Line::from(Span::styled(
        format!("      ... {} more lines", hidden),
        theme.fg(theme.dim),
    )));
    result
}

use std::time::{Duration, Instant};

/// Format a duration for human display.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 0.1 {
        format!("{:.0}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{:.1}s", secs)
    } else if secs < 3600.0 {
        let m = (secs / 60.0).floor() as u64;
        let s = (secs % 60.0).floor() as u64;
        format!("{}m {}s", m, s)
    } else {
        let h = (secs / 3600.0).floor() as u64;
        let m = ((secs % 3600.0) / 60.0).floor() as u64;
        format!("{}h {}m", h, m)
    }
}

/// Trait for rendering tool calls and results.
#[allow(clippy::too_many_arguments)]
pub trait ToolRenderer {
    /// Render an actively running tool (output streaming in).
    fn render_active(
        &self,
        name: &str,
        args: &Value,
        output_lines: &[String],
        started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>>;

    /// Render a completed tool.
    fn render_complete(
        &self,
        name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>>;
}

/// Registry of tool renderers.
pub struct RendererRegistry {
    renderers: HashMap<String, Box<dyn ToolRenderer>>,
    default: DefaultRenderer,
}

impl RendererRegistry {
    pub fn new() -> Self {
        let mut renderers: HashMap<String, Box<dyn ToolRenderer>> = HashMap::new();
        renderers.insert("bash".into(), Box::new(BashRenderer));
        renderers.insert("edit".into(), Box::new(EditRenderer));
        renderers.insert("read".into(), Box::new(ReadRenderer));
        renderers.insert("write".into(), Box::new(WriteRenderer));
        Self {
            renderers,
            default: DefaultRenderer,
        }
    }

    pub fn get(&self, name: &str) -> &dyn ToolRenderer {
        self.renderers
            .get(name)
            .map(|r| r.as_ref())
            .unwrap_or(&self.default)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render a header, wrapping long info text across multiple lines.
/// First line: " name info...", continuation lines indented to align.
fn header_lines(
    name: &str,
    info: &str,
    theme: &Theme,
    bg: Style,
    width: u16,
) -> Vec<Line<'static>> {
    let title_style = bg
        .fg(theme.tool_title.to_ratatui())
        .add_modifier(Modifier::BOLD);
    let info_style = bg.fg(theme.tool_output.to_ratatui());

    // " name " prefix occupies this many columns
    let prefix_len = 1 + name.len() + 1; // space + name + space
    let usable = (width as usize).saturating_sub(prefix_len);

    if usable == 0 || info.len() <= usable {
        return vec![Line::from(vec![
            Span::styled(format!(" {}", name), title_style),
            Span::styled(format!(" {}", info), info_style),
        ])];
    }

    // Wrap info into chunks of `usable` width
    let mut lines = Vec::new();
    let mut remaining = info;
    let indent: String = " ".repeat(prefix_len);
    let mut first = true;
    while !remaining.is_empty() {
        let split = remaining.len().min(usable);
        let chunk = &remaining[..split];
        remaining = &remaining[split..];
        if first {
            lines.push(Line::from(vec![
                Span::styled(format!(" {}", name), title_style),
                Span::styled(format!(" {}", chunk), info_style),
            ]));
            first = false;
        } else {
            lines.push(Line::from(Span::styled(
                format!("{}{}", indent, chunk),
                info_style,
            )));
        }
    }
    lines
}

/// Render output lines with clamping and indentation.
fn output_lines(lines: &[String], theme: &Theme, bg: Style, max: usize) -> Vec<Line<'static>> {
    let styled: Vec<Line<'static>> = lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                format!("  {}", l),
                bg.fg(theme.tool_output.to_ratatui()),
            ))
        })
        .collect();
    clamp_lines(&styled, max, theme)
}

/// Pad lines with bg to full width, add top/bottom padding.
pub fn wrap_tool_block(lines: Vec<Line<'static>>, bg: Style, width: u16) -> Vec<Line<'static>> {
    let mut result = Vec::with_capacity(lines.len() + 2);
    result.push(Line::from(Span::styled(" ", bg))); // top padding
    result.extend(lines);
    result.push(Line::from(Span::styled(" ", bg))); // bottom padding
    crate::message::fill_bg(&mut result, bg, width);
    result
}

// ---------------------------------------------------------------------------
// Default renderer
// ---------------------------------------------------------------------------

pub struct DefaultRenderer;

impl ToolRenderer for DefaultRenderer {
    fn render_active(
        &self,
        name: &str,
        args: &Value,
        output: &[String],
        _started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let args_preview = truncate_str(&args.to_string(), 80);
        let mut lines = header_lines(name, &args_preview, theme, bg, width);
        if !output.is_empty() {
            lines.extend(output_lines(output, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        _duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let args_preview = truncate_str(&args.to_string(), 80);
        let mut lines = header_lines(name, &args_preview, theme, bg, width);
        if !output.is_empty() {
            let out: Vec<String> = output.lines().map(String::from).collect();
            lines.extend(output_lines(&out, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }
}

// ---------------------------------------------------------------------------
// Bash renderer
// ---------------------------------------------------------------------------

struct BashRenderer;

impl BashRenderer {
    fn bash_header(
        args: &Value,
        suffix: &str,
        theme: &Theme,
        bg: Style,
        width: u16,
    ) -> Vec<Line<'static>> {
        let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("?");
        let timeout = args.get("timeout").and_then(|t| t.as_u64());
        let mut info = format!("$ {}", command);
        if let Some(t) = timeout {
            info.push_str(&format!(" (timeout {}s)", t));
        }
        if !suffix.is_empty() {
            info.push_str(&format!(" {}", suffix));
        }
        header_lines("bash", &info, theme, bg, width)
    }
}

impl ToolRenderer for BashRenderer {
    fn render_active(
        &self,
        _name: &str,
        args: &Value,
        output: &[String],
        started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let elapsed = format_duration(started_at.elapsed());
        let mut lines =
            BashRenderer::bash_header(args, &format!("(elapsed {})", elapsed), theme, bg, width);
        if !output.is_empty() {
            lines.extend(output_lines(output, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        _name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let mut lines = BashRenderer::bash_header(args, "", theme, bg, width);

        // Output lines
        let output_text = output.trim_end();
        if output_text.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no output)",
                bg.fg(theme.dim.to_ratatui()),
            )));
        } else {
            // Strip trailing "(exit code: N)" from output since we show it separately
            let clean = if let Some(code) = extract_exit_code(output_text) {
                output_text
                    .strip_suffix(&format!("\n(exit code: {})", code))
                    .unwrap_or(output_text)
            } else {
                output_text
            };
            if clean.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (no output)",
                    bg.fg(theme.dim.to_ratatui()),
                )));
            } else {
                let out: Vec<String> = clean.lines().map(String::from).collect();
                lines.extend(output_lines(&out, theme, bg, DEFAULT_MAX_LINES));
            }
        }

        // Metadata footer
        let meta_style = bg.fg(theme.dim.to_ratatui());
        if let Some(code) = extract_exit_code(output_text)
            && code != 0
        {
            lines.push(Line::from(Span::styled(
                format!("  Command exited with code {}", code),
                meta_style,
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("  Took {}", format_duration(duration)),
            meta_style,
        )));

        wrap_tool_block(lines, bg, width)
    }
}

/// Try to extract exit code from bash output ("(exit code: N)" at end).
fn extract_exit_code(output: &str) -> Option<i32> {
    let trimmed = output.trim_end();
    if let Some(idx) = trimmed.rfind("(exit code: ") {
        let rest = &trimmed[idx + "(exit code: ".len()..];
        rest.strip_suffix(')').and_then(|s| s.parse().ok())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Edit renderer
// ---------------------------------------------------------------------------

struct EditRenderer;

impl ToolRenderer for EditRenderer {
    fn render_active(
        &self,
        _name: &str,
        args: &Value,
        _output: &[String],
        _started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let lines = header_lines("edit", path, theme, bg, width);
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        _name: &str,
        args: &Value,
        _output: &str,
        is_error: bool,
        _duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");

        let mut lines = header_lines("edit", path, theme, bg, width);

        // Render a diff-like view from edits
        if let Some(edits) = args.get("edits").and_then(|e| e.as_array()) {
            for edit in edits {
                let old = edit.get("oldText").and_then(|t| t.as_str()).unwrap_or("");
                let new = edit.get("newText").and_then(|t| t.as_str()).unwrap_or("");
                let diff_lines = render_edit_diff(old, new, theme, bg);
                let clamped = clamp_lines(&diff_lines, DEFAULT_MAX_LINES, theme);
                lines.extend(clamped);
            }
        }

        wrap_tool_block(lines, bg, width)
    }
}

/// Render a simple diff between old and new text.
fn render_edit_diff(old: &str, new: &str, theme: &Theme, bg: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let ellipsis_style = bg.fg(theme.dim.to_ratatui());
    let context_style = bg.fg(theme.tool_output.to_ratatui());
    let added_style = bg.fg(theme.success.to_ratatui());
    let removed_style = bg.fg(theme.error.to_ratatui());

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Show context + removed lines
    lines.push(Line::from(Span::styled("      ...", ellipsis_style)));
    for (i, line) in old_lines.iter().enumerate() {
        let prefix = format!("  {:>4} ", i + 1);
        lines.push(Line::from(vec![
            Span::styled(prefix, context_style),
            Span::styled(format!("-{}", line), removed_style),
        ]));
    }

    // Show added lines
    for (i, line) in new_lines.iter().enumerate() {
        let prefix = format!("  {:>4} ", i + 1);
        lines.push(Line::from(vec![
            Span::styled(prefix, context_style),
            Span::styled(format!("+{}", line), added_style),
        ]));
    }
    lines.push(Line::from(Span::styled("      ...", ellipsis_style)));

    lines
}

// ---------------------------------------------------------------------------
// Read renderer
// ---------------------------------------------------------------------------

struct ReadRenderer;

impl ToolRenderer for ReadRenderer {
    fn render_active(
        &self,
        _name: &str,
        args: &Value,
        _output: &[String],
        _started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let mut info = path.to_string();
        if let Some(offset) = args.get("offset").and_then(|o| o.as_u64()) {
            info.push_str(&format!(" (offset: {})", offset));
        }
        let lines = header_lines("read", &info, theme, bg, width);
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        _name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        _duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let line_count = output.lines().count();
        let info = format!("{} ({} lines)", path, line_count);
        let mut lines = header_lines("read", &info, theme, bg, width);
        if !output.is_empty() {
            let out: Vec<String> = output.lines().map(String::from).collect();
            lines.extend(output_lines(&out, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }
}

// ---------------------------------------------------------------------------
// Write renderer
// ---------------------------------------------------------------------------

struct WriteRenderer;

impl ToolRenderer for WriteRenderer {
    fn render_active(
        &self,
        _name: &str,
        args: &Value,
        _output: &[String],
        _started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let lines = header_lines("write", path, theme, bg, width);
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        _name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        _duration: Duration,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let content_len = args
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        let info = if is_error {
            format!("{} (error)", path)
        } else {
            format!("{} ({} bytes)", path, content_len)
        };
        let mut lines = header_lines("write", &info, theme, bg, width);
        if is_error && !output.is_empty() {
            let out: Vec<String> = output.lines().map(String::from).collect();
            lines.extend(output_lines(&out, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max])
    } else {
        s.to_string()
    }
}
