//! Read tool — read file contents.

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "read".into(),
            description: "Read the contents of a file. Supports offset/limit for large files."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: None,
    }
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    _cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    let Some(path_str) = args.get("path").and_then(|p| p.as_str()) else {
        return ToolOutput::error("missing 'path' argument");
    };

    let path = super::resolve_path(cwd, path_str);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to read {}: {}", path.display(), e)),
    };

    let offset = args
        .get("offset")
        .and_then(|o| o.as_u64())
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(|l| l.as_u64())
        .map(|l| l as usize);

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = (offset - 1).min(total);
    let end = match limit {
        Some(l) => (start + l).min(total),
        None => total,
    };

    let selected = &lines[start..end];
    let mut result = selected.join("\n");

    if end < total {
        result.push_str(&format!(
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            total - end,
            end + 1,
        ));
    }

    let summary = if start == 0 && end == total {
        format!("read: {} ({} lines)", path_str, total)
    } else {
        format!(
            "read: {} (lines {}-{}, {} total)",
            path_str,
            start + 1,
            end,
            total
        )
    };

    ToolOutput::text(result).with_summary(summary)
}
