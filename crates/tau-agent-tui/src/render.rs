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

/// Like `format_duration` but rounds to whole seconds below the minute
/// boundary — so the ticker reads `0s, 1s, 2s, …` instead of
/// `1.0s, 1.1s, 1.2s`. Used by the TUI's "Working... Xs" spinner counter
/// where fractional seconds would jitter.
pub(crate) fn format_duration_whole_seconds(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        format!("{}s", total)
    } else if total < 3600 {
        let m = total / 60;
        let s = total % 60;
        format!("{}m {}s", m, s)
    } else {
        let h = total / 3600;
        let m = (total % 3600) / 60;
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
        duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
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
        for name in TASK_TOOL_NAMES {
            renderers.insert((*name).into(), Box::new(TaskRenderer));
        }
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

    // " name " + leading space of info span = total columns before info text
    let prefix_len = 1 + name.len() + 1 + 1; // space + name + space + info leading space
    let usable = (width as usize).saturating_sub(prefix_len);

    /// Display width of a string (excludes control characters).
    fn display_width(s: &str) -> usize {
        s.chars().filter(|c| !c.is_control()).count()
    }

    if usable == 0 || display_width(info) <= usable {
        return vec![Line::from(vec![
            Span::styled(format!(" {}", name), title_style),
            Span::styled(format!(" {}", info), info_style),
        ])];
    }

    // Split on newlines, then wrap each sub-line to `usable` display width.
    let mut lines = Vec::new();
    let indent: String = " ".repeat(prefix_len);
    let mut first = true;

    for info_line in info.split('\n') {
        let mut remaining = info_line;
        while !remaining.is_empty() {
            let dw = display_width(remaining);
            let split = if dw <= usable {
                remaining.len()
            } else {
                // Find byte offset of the usable-th display character
                let byte_limit = remaining
                    .char_indices()
                    .filter(|(_, c)| !c.is_control())
                    .nth(usable)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len());
                // Try word boundary within that range
                remaining[..byte_limit]
                    .rfind(' ')
                    .map(|i| i + 1)
                    .unwrap_or(byte_limit)
            };
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
    }
    lines
}

/// If a summary is available and the block is not expanded, render just the
/// summary as a collapsed one-liner.  Returns `Some(lines)` if collapsed,
/// `None` if the caller should fall through to full rendering.
fn collapsed_summary(
    summary: Option<&str>,
    expanded: bool,
    is_error: bool,
    theme: &Theme,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let summary = summary?;
    if expanded {
        return None;
    }
    let bg = if is_error {
        theme.tool_error_style()
    } else {
        theme.tool_success_style()
    };
    let title_style = bg
        .fg(theme.tool_title.to_ratatui())
        .add_modifier(Modifier::BOLD);
    let lines = vec![Line::from(Span::styled(
        format!(" {}", summary),
        title_style,
    ))];
    Some(wrap_tool_block(lines, bg, width))
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
        _duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        if let Some(lines) = collapsed_summary(summary, expanded, is_error, theme, width) {
            return lines;
        }
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
        duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        if let Some(lines) = collapsed_summary(summary, expanded, is_error, theme, width) {
            return lines;
        }
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
            format!(
                "  Took {}",
                duration.map(format_duration).unwrap_or_else(|| "—".into())
            ),
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
        _duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        if let Some(lines) = collapsed_summary(summary, expanded, is_error, theme, width) {
            return lines;
        }
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
        _duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        if let Some(lines) = collapsed_summary(summary, expanded, is_error, theme, width) {
            return lines;
        }
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
        _duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        if let Some(lines) = collapsed_summary(summary, expanded, is_error, theme, width) {
            return lines;
        }
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
// Task renderer
// ---------------------------------------------------------------------------

/// All registered task_* tool names. Kept in sync with
/// `crates/tau-agent-plugin-tasks/src/tasks.rs`.
const TASK_TOOL_NAMES: &[&str] = &[
    "task_create",
    "task_get",
    "task_list",
    "task_assign",
    "task_update",
    "task_message",
    "task_message_edit",
    "task_relate",
    "task_search",
    "task_schedule",
    "task_merge",
    "task_dispatch",
    "task_status",
    "task_overview",
];

/// Field names treated as long-form text bodies (rendered with real newlines
/// in expanded mode, truncated for the collapsed one-liner).
const TASK_LONG_TEXT_FIELDS: &[&str] = &["content", "message"];

/// Stable ordering for known scalar metadata keys in expanded mode.
const TASK_SCALAR_ORDER: &[&str] = &[
    "id",
    "task_id",
    "parent_id",
    "message_id",
    "from_task",
    "to_task",
    "relation",
    "title",
    "state",
    "priority",
    "branch",
    "merge_target",
    "tags",
    "hold",
    "skip_review",
    "require_approval",
    "sandbox_profile",
    "initial_state",
    "affected_files",
    "project",
    "query",
    "session_id",
    "limit",
    "recent_limit",
];

struct TaskRenderer;

impl TaskRenderer {
    /// True if `field` should be rendered as a long-form text body.
    /// A field qualifies when its value is a string AND either the field
    /// name is in `TASK_LONG_TEXT_FIELDS`, or the value contains a newline.
    fn is_long_text(field: &str, value: &Value) -> bool {
        let Some(s) = value.as_str() else {
            return false;
        };
        if TASK_LONG_TEXT_FIELDS.contains(&field) {
            return true;
        }
        s.contains('\n')
    }

    /// Render a single scalar value (not an object/long-text body) as a
    /// short string. Strings come back unquoted; arrays of scalars are
    /// comma-joined; nested objects fall back to compact JSON.
    fn format_scalar(value: &Value) -> String {
        match value {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            Value::Array(arr) => {
                let parts: Vec<String> = arr
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                parts.join(", ")
            }
            Value::Object(_) => value.to_string(),
        }
    }

    /// Build a compact, human-readable one-line summary. Never returns a
    /// raw JSON dump: long-text bodies are truncated on the unescaped
    /// string so the user sees the first words instead of escape sequences.
    fn collapsed_info(name: &str, args: &Value) -> String {
        let obj = args.as_object();

        // Helper to fetch primary id-ish fields in priority order.
        let id_field = |keys: &[&str]| -> Option<String> {
            obj.and_then(|o| {
                keys.iter().find_map(|k| {
                    o.get(*k).and_then(|v| match v {
                        Value::Number(n) => Some(n.to_string()),
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                })
            })
        };

        let get_str = |k: &str| -> Option<String> {
            obj.and_then(|o| o.get(k))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };

        match name {
            "task_message" => {
                let id = id_field(&["id"]);
                let body = get_str("content").unwrap_or_default();
                format_id_with_text(&id, &body)
            }
            "task_message_edit" => {
                let task_id = id_field(&["task_id"]);
                let msg_id = id_field(&["message_id"]);
                let body = get_str("content").unwrap_or_default();
                let id_part = match (task_id.as_deref(), msg_id.as_deref()) {
                    (Some(t), Some(m)) => format!("#{} msg#{}", t, m),
                    (Some(t), None) => format!("#{}", t),
                    _ => "#—".to_string(),
                };
                if body.is_empty() {
                    id_part
                } else {
                    format!("{} — {}", id_part, quote_oneline(&body, 60))
                }
            }
            "task_create" => {
                let title = get_str("title").unwrap_or_default();
                if title.is_empty() {
                    "#—".to_string()
                } else {
                    format!("#— — {}", quote_oneline(&title, 60))
                }
            }
            "task_update" => {
                let id = id_field(&["id"]);
                let mut bits: Vec<String> = Vec::new();
                if let Some(state) = get_str("state") {
                    bits.push(format!("state={}", state));
                }
                if let Some(o) = obj
                    && let Some(hold) = o.get("hold").and_then(|v| v.as_bool())
                {
                    bits.push(format!("hold={}", hold));
                }
                if let Some(prio) = obj.and_then(|o| o.get("priority")) {
                    bits.push(format!("priority={}", Self::format_scalar(prio)));
                }
                let id_part = match id.as_deref() {
                    Some(s) => format!("#{}", s),
                    None => "#—".to_string(),
                };
                if bits.is_empty() {
                    id_part
                } else {
                    format!("{} {}", id_part, bits.join(" "))
                }
            }
            "task_get" | "task_assign" | "task_dispatch" | "task_merge" => {
                let id = id_field(&["id"]);
                match id.as_deref() {
                    Some(s) => format!("#{}", s),
                    None => "#—".to_string(),
                }
            }
            "task_relate" => {
                let from = id_field(&["from_task"]);
                let to = id_field(&["to_task"]);
                let relation = get_str("relation").unwrap_or_else(|| "related".to_string());
                match (from, to) {
                    (Some(f), Some(t)) => format!("#{} -[{}]-> #{}", f, relation, t),
                    _ => relation,
                }
            }
            "task_search" => {
                let q = get_str("query").unwrap_or_default();
                if q.is_empty() {
                    String::new()
                } else {
                    format!("query={}", quote_oneline(&q, 60))
                }
            }
            "task_list" => {
                let mut bits: Vec<String> = Vec::new();
                if let Some(state) = get_str("state") {
                    bits.push(format!("state={}", state));
                }
                if let Some(parent) = id_field(&["parent_id"]) {
                    bits.push(format!("parent=#{}", parent));
                }
                if let Some(tag) = get_str("tag") {
                    bits.push(format!("tag={}", tag));
                }
                bits.join(" ")
            }
            "task_schedule" | "task_status" | "task_overview" => {
                if let Some(p) = get_str("project") {
                    format!("project={}", p)
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        }
    }
}

/// Format `"#<id> — \"<text>...\""` style summary, falling back gracefully
/// when either piece is missing.
fn format_id_with_text(id: &Option<String>, body: &str) -> String {
    let id_part = match id.as_deref() {
        Some(s) => format!("#{}", s),
        None => "#—".to_string(),
    };
    if body.is_empty() {
        id_part
    } else {
        format!("{} — {}", id_part, quote_oneline(body, 60))
    }
}

/// Take a possibly-multiline string and return a quoted single-line
/// preview truncated to ~`max` chars. Uses real characters (no JSON escape
/// of `\n`); newlines are replaced by a single space.
fn quote_oneline(s: &str, max: usize) -> String {
    let collapsed: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    let trimmed = collapsed.trim();
    let truncated = if trimmed.chars().count() > max {
        let mut out: String = trimmed.chars().take(max).collect();
        out.push('…');
        out
    } else {
        trimmed.to_string()
    };
    format!("\"{}\"", truncated)
}

/// Stable iteration order for an args object: known keys in
/// `TASK_SCALAR_ORDER` first (in that order), then any remaining keys in
/// the object's insertion order.
fn ordered_scalar_keys(obj: &serde_json::Map<String, Value>) -> Vec<&str> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<&str> = Vec::new();
    for known in TASK_SCALAR_ORDER {
        if obj.contains_key(*known) {
            out.push(*known);
            seen.insert(*known);
        }
    }
    for k in obj.keys() {
        if !seen.contains(k.as_str()) {
            out.push(k.as_str());
        }
    }
    out
}

impl ToolRenderer for TaskRenderer {
    fn render_active(
        &self,
        name: &str,
        args: &Value,
        output: &[String],
        started_at: Instant,
        theme: &Theme,
        width: u16,
    ) -> Vec<Line<'static>> {
        let bg = theme.tool_pending_style();
        let elapsed = format_duration(started_at.elapsed());
        let summary = TaskRenderer::collapsed_info(name, args);
        let info = if summary.is_empty() {
            format!("(elapsed {})", elapsed)
        } else {
            format!("{} (elapsed {})", summary, elapsed)
        };
        let mut lines = header_lines(name, &info, theme, bg, width);
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
        _duration: Option<Duration>,
        theme: &Theme,
        width: u16,
        summary: Option<&str>,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        let bg = if is_error {
            theme.tool_error_style()
        } else {
            theme.tool_success_style()
        };

        // Collapsed (default): server-supplied summary if any, else our
        // synthesised one-liner. Never a JSON dump.
        if !expanded {
            let title_style = bg
                .fg(theme.tool_title.to_ratatui())
                .add_modifier(Modifier::BOLD);
            if let Some(s) = summary {
                let lines = vec![Line::from(Span::styled(format!(" {}", s), title_style))];
                return wrap_tool_block(lines, bg, width);
            }
            let info = TaskRenderer::collapsed_info(name, args);
            let lines = header_lines(name, &info, theme, bg, width);
            return wrap_tool_block(lines, bg, width);
        }

        // Expanded: metadata block first, then long-text bodies, then
        // tool output (if any).
        let header_info = TaskRenderer::collapsed_info(name, args);
        let mut lines = header_lines(name, &header_info, theme, bg, width);

        let output_style = bg.fg(theme.tool_output.to_ratatui());
        let title_style = bg
            .fg(theme.tool_title.to_ratatui())
            .add_modifier(Modifier::BOLD);

        if let Some(obj) = args.as_object() {
            // Scalar metadata: every non-long-text field, in stable order.
            let mut scalar_pairs: Vec<(String, String)> = Vec::new();
            for key in ordered_scalar_keys(obj) {
                let value = match obj.get(key) {
                    Some(v) => v,
                    None => continue,
                };
                if TaskRenderer::is_long_text(key, value) {
                    continue;
                }
                scalar_pairs.push((key.to_string(), TaskRenderer::format_scalar(value)));
            }
            for (k, v) in &scalar_pairs {
                lines.push(Line::from(Span::styled(
                    format!("  {}: {}", k, v),
                    output_style,
                )));
            }

            // Long-text bodies.
            let mut body_keys: Vec<&str> = Vec::new();
            // Preferred order first.
            for known in TASK_LONG_TEXT_FIELDS {
                if let Some(v) = obj.get(*known)
                    && TaskRenderer::is_long_text(known, v)
                {
                    body_keys.push(*known);
                }
            }
            // Then any other field whose value is a multi-line string.
            for (k, v) in obj.iter() {
                if TASK_LONG_TEXT_FIELDS.contains(&k.as_str()) {
                    continue;
                }
                if TaskRenderer::is_long_text(k, v) {
                    body_keys.push(k.as_str());
                }
            }

            for key in body_keys {
                let body = match obj.get(key).and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => continue,
                };
                if !scalar_pairs.is_empty() || lines.len() > 1 {
                    lines.push(Line::from(Span::styled(" ", bg)));
                }
                lines.push(Line::from(Span::styled(format!(" {}:", key), title_style)));
                let body_lines: Vec<String> = body.split('\n').map(String::from).collect();
                lines.extend(output_lines(
                    &body_lines,
                    theme,
                    bg,
                    DEFAULT_MAX_LINES.max(body_lines.len()),
                ));
            }
        }

        // Tool output / error result.
        if !output.is_empty() {
            // Try to pretty-print JSON for readability; fall back to raw.
            let pretty = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| output.to_string());
            let out: Vec<String> = pretty.lines().map(String::from).collect();
            lines.push(Line::from(Span::styled(" ", bg)));
            lines.push(Line::from(Span::styled(" output:", title_style)));
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
        format!("{}...", tau_agent_lib::truncate_str(s, max))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use std::time::Duration;

    fn test_theme() -> Theme {
        theme::dark()
    }

    // -- format_duration unit tests --

    #[test]
    fn format_duration_milliseconds() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_millis(1200)), "1.2s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m");
    }

    // -- format_duration_whole_seconds unit tests --

    #[test]
    fn whole_seconds_zero() {
        assert_eq!(
            format_duration_whole_seconds(Duration::from_millis(0)),
            "0s"
        );
    }

    #[test]
    fn whole_seconds_floors_fractional() {
        // 1.9s should display as "1s", matching how the spinner ticks over
        // integer-second boundaries rather than showing "1.9s".
        assert_eq!(
            format_duration_whole_seconds(Duration::from_millis(1_900)),
            "1s"
        );
    }

    #[test]
    fn whole_seconds_sub_minute() {
        assert_eq!(format_duration_whole_seconds(Duration::from_secs(1)), "1s");
        assert_eq!(
            format_duration_whole_seconds(Duration::from_secs(59)),
            "59s"
        );
    }

    #[test]
    fn whole_seconds_minute_boundary() {
        assert_eq!(
            format_duration_whole_seconds(Duration::from_secs(60)),
            "1m 0s"
        );
    }

    #[test]
    fn whole_seconds_minutes() {
        assert_eq!(
            format_duration_whole_seconds(Duration::from_secs(125)),
            "2m 5s"
        );
    }

    #[test]
    fn whole_seconds_hour() {
        assert_eq!(
            format_duration_whole_seconds(Duration::from_secs(3661)),
            "1h 1m"
        );
    }

    // -- BashRenderer::render_complete tests --

    #[test]
    fn bash_render_complete_shows_duration() {
        let renderer = BashRenderer;
        let theme = test_theme();
        let lines = renderer.render_complete(
            "bash",
            &serde_json::json!({"command": "ls"}),
            "file1\nfile2",
            false,
            Some(Duration::from_millis(1200)),
            &theme,
            80,
            None,
            true,
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Took 1.2s"),
            "Expected 'Took 1.2s' in: {}",
            text
        );
    }

    #[test]
    fn bash_render_complete_shows_dash_for_unknown_duration() {
        let renderer = BashRenderer;
        let theme = test_theme();
        let lines = renderer.render_complete(
            "bash",
            &serde_json::json!({"command": "ls"}),
            "file1\nfile2",
            false,
            None,
            &theme,
            80,
            None,
            true,
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Took —"), "Expected 'Took —' in: {}", text);
        assert!(
            !text.contains("Took 0ms"),
            "Should not show 'Took 0ms' in: {}",
            text
        );
    }

    #[test]
    fn bash_render_complete_error_shows_exit_code() {
        let renderer = BashRenderer;
        let theme = test_theme();
        let lines = renderer.render_complete(
            "bash",
            &serde_json::json!({"command": "false"}),
            "something failed\n(exit code: 1)",
            true,
            Some(Duration::from_millis(50)),
            &theme,
            80,
            None,
            true,
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("exited with code 1"),
            "Expected exit code info in: {}",
            text
        );
        assert!(
            text.contains("Took 50ms"),
            "Expected 'Took 50ms' in: {}",
            text
        );
    }

    #[test]
    fn bash_render_complete_no_output() {
        let renderer = BashRenderer;
        let theme = test_theme();
        let lines = renderer.render_complete(
            "bash",
            &serde_json::json!({"command": "true"}),
            "",
            false,
            Some(Duration::from_millis(5)),
            &theme,
            80,
            None,
            true,
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("(no output)"),
            "Expected '(no output)' in: {}",
            text
        );
        assert!(
            text.contains("Took 5ms"),
            "Expected 'Took 5ms' in: {}",
            text
        );
    }

    // -- TaskRenderer tests --

    fn render_task(name: &str, args: Value, expanded: bool) -> String {
        let renderer = TaskRenderer;
        let theme = test_theme();
        let lines =
            renderer.render_complete(name, &args, "", false, None, &theme, 80, None, expanded);
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn task_renderer_collapsed_one_liner_for_task_message() {
        let text = render_task(
            "task_message",
            serde_json::json!({"id": 702, "content": "## Foo\nbar"}),
            false,
        );
        assert!(text.contains("#702"), "missing id: {}", text);
        assert!(text.contains("## Foo"), "missing content head: {}", text);
        // No JSON braces or escaped newlines:
        assert!(!text.contains('{'), "unexpected JSON brace: {}", text);
        assert!(!text.contains('}'), "unexpected JSON brace: {}", text);
        assert!(
            !text.contains("\\n"),
            "unexpected escape sequence: {}",
            text
        );
    }

    #[test]
    fn task_renderer_expanded_renders_content_with_real_newlines() {
        let text = render_task(
            "task_message",
            serde_json::json!({"id": 702, "content": "## Foo\nbar"}),
            true,
        );
        // Look for distinct lines containing "## Foo" and "bar".
        let lines: Vec<&str> = text.split('\n').collect();
        let foo_line = lines.iter().find(|l| l.contains("## Foo"));
        let bar_line = lines.iter().find(|l| l.trim() == "bar");
        assert!(foo_line.is_some(), "no '## Foo' line in:\n{}", text);
        assert!(bar_line.is_some(), "no 'bar' line in:\n{}", text);
        // No literal escape sequences:
        for l in &lines {
            assert!(!l.contains("\\n"), "escape leaked through: {:?}", l);
        }
    }

    #[test]
    fn task_renderer_expanded_metadata_before_body() {
        let text = render_task(
            "task_update",
            serde_json::json!({"id": 702, "state": "ready", "content": "hi"}),
            true,
        );
        let lines: Vec<&str> = text.split('\n').collect();
        let state_idx = lines
            .iter()
            .position(|l| l.contains("state: ready"))
            .unwrap_or_else(|| panic!("no 'state: ready' line in:\n{}", text));
        let body_idx = lines
            .iter()
            .position(|l| l.trim() == "hi")
            .unwrap_or_else(|| panic!("no 'hi' body line in:\n{}", text));
        assert!(
            state_idx < body_idx,
            "metadata should come before body, got state at {} body at {}:\n{}",
            state_idx,
            body_idx,
            text
        );
    }

    #[test]
    fn task_renderer_falls_back_for_unknown_keys() {
        let text = render_task(
            "task_update",
            serde_json::json!({"id": 1, "weird_field": "x"}),
            true,
        );
        assert!(
            text.contains("weird_field: x"),
            "expected 'weird_field: x' in:\n{}",
            text
        );
    }

    #[test]
    fn task_renderer_handles_all_registered_task_tool_names() {
        // Verify every documented task tool name resolves to a non-default
        // renderer by comparing rendered output against DefaultRenderer
        // for the same args/output.
        let registry = RendererRegistry::new();
        let theme = test_theme();
        let args = serde_json::json!({"id": 7, "content": "hello\nworld"});
        let default = DefaultRenderer;

        for name in TASK_TOOL_NAMES {
            let task_lines = registry
                .get(name)
                .render_complete(name, &args, "", false, None, &theme, 80, None, true);
            let default_lines =
                default.render_complete(name, &args, "", false, None, &theme, 80, None, true);
            let task_text: String = task_lines.iter().map(|l| l.to_string()).collect();
            let default_text: String = default_lines.iter().map(|l| l.to_string()).collect();
            assert_ne!(
                task_text, default_text,
                "renderer for {} matched DefaultRenderer output",
                name
            );
        }
    }

    #[test]
    fn task_renderer_collapsed_task_create_uses_title() {
        let text = render_task(
            "task_create",
            serde_json::json!({"title": "TUI: show timing"}),
            false,
        );
        assert!(
            text.contains("TUI: show timing"),
            "expected title in collapsed view: {}",
            text
        );
        assert!(!text.contains('{'), "no JSON dump expected: {}", text);
    }

    #[test]
    fn task_renderer_expanded_omits_long_text_field_from_metadata() {
        // 'content' is a known long-text field; it must NOT appear as
        // 'content: ...' in the scalar metadata block.
        let text = render_task(
            "task_message",
            serde_json::json!({"id": 9, "content": "line1\nline2"}),
            true,
        );
        assert!(
            !text.contains("content: line1"),
            "content scalar leaked into metadata block:\n{}",
            text
        );
        // But the heading 'content:' (with no value on the same line)
        // should be present as the body section heading.
        assert!(
            text.lines().any(|l| l.trim() == "content:"),
            "missing 'content:' body heading in:\n{}",
            text
        );
    }
}
