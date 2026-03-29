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

/// Trait for rendering tool calls and results.
pub trait ToolRenderer {
    /// Render an actively running tool (output streaming in).
    fn render_active(
        &self,
        args: &Value,
        output_lines: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>>;

    /// Render a completed tool.
    fn render_complete(
        &self,
        args: &Value,
        output: &str,
        is_error: bool,
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

/// Render a header line: "tool_name key_info"
fn header_line(name: &str, info: &str, theme: &Theme, bg: Style) -> Line<'static> {
    let title_style = bg
        .fg(theme.tool_title.to_ratatui())
        .add_modifier(Modifier::BOLD);
    let info_style = bg.fg(theme.tool_output.to_ratatui());
    Line::from(vec![
        Span::styled(format!(" {}", name), title_style),
        Span::styled(format!(" {}", info), info_style),
    ])
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
        args: &Value,
        output: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let args_preview = truncate_str(&args.to_string(), 80);
        let mut lines = vec![header_line("tool", &args_preview, theme, bg)];
        if !output.is_empty() {
            lines.extend(output_lines(output, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        args: &Value,
        output: &str,
        is_error: bool,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let args_preview = truncate_str(&args.to_string(), 80);
        let mut lines = vec![header_line("tool", &args_preview, theme, bg)];
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

impl ToolRenderer for BashRenderer {
    fn render_active(
        &self,
        args: &Value,
        output: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("?");
        let mut lines = vec![header_line("bash", command, theme, bg)];
        if !output.is_empty() {
            lines.extend(output_lines(output, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        args: &Value,
        output: &str,
        is_error: bool,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("?");
        let mut lines = vec![header_line("bash", command, theme, bg)];
        if !output.is_empty() {
            let out: Vec<String> = output.lines().map(String::from).collect();
            lines.extend(output_lines(&out, theme, bg, DEFAULT_MAX_LINES));
        }
        wrap_tool_block(lines, bg, width)
    }
}

// ---------------------------------------------------------------------------
// Edit renderer
// ---------------------------------------------------------------------------

struct EditRenderer;

impl ToolRenderer for EditRenderer {
    fn render_active(
        &self,
        args: &Value,
        _output: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let lines = vec![header_line("edit", path, theme, bg)];
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        args: &Value,
        _output: &str,
        is_error: bool,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");

        let mut lines = vec![header_line("edit", path, theme, bg)];

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
        args: &Value,
        _output: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let mut info = path.to_string();
        if let Some(offset) = args.get("offset").and_then(|o| o.as_u64()) {
            info.push_str(&format!(" (offset: {})", offset));
        }
        let lines = vec![header_line("read", &info, theme, bg)];
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        args: &Value,
        output: &str,
        is_error: bool,
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
        let mut lines = vec![header_line("read", &info, theme, bg)];
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
        args: &Value,
        _output: &[String],
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let lines = vec![header_line("write", path, theme, bg)];
        wrap_tool_block(lines, bg, width)
    }

    fn render_complete(
        &self,
        args: &Value,
        output: &str,
        is_error: bool,
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
        let mut lines = vec![header_line("write", &info, theme, bg)];
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
