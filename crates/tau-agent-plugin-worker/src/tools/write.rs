//! Write tool — create or overwrite files.

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "write".into(),
            description:
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
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
    let Some(content) = args.get("content").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'content' argument");
    };

    let path = super::resolve_path(cwd, path_str);

    // Create parent directories
    if let Some(parent) = path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolOutput::error(format!("failed to create directory: {}", e));
    }

    match std::fs::write(&path, content) {
        Ok(()) => {
            let line_count = content.lines().count();
            let summary = format!(
                "write: {} ({} lines, {} bytes)",
                path_str,
                line_count,
                content.len()
            );
            ToolOutput::text(format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                path.display()
            ))
            .with_summary(summary)
        }
        Err(e) => ToolOutput::error(format!("failed to write {}: {}", path.display(), e)),
    }
}
