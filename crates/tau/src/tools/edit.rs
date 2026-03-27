//! Edit tool — surgical find-and-replace in files.

use super::{ToolDef, ToolOutput};
use crate::types::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "edit".into(),
            description:
                "Edit a file by replacing exact text. The old_text must match exactly (including whitespace and newlines). Use this for precise, surgical edits."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to find and replace (must match exactly)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "New text to replace the old text with"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        execute: Box::new(execute),
    }
}

fn resolve_path(cwd: &str, path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(p)
    }
}

fn execute(args: serde_json::Value, cwd: &str) -> ToolOutput {
    let Some(path_str) = args.get("path").and_then(|p| p.as_str()) else {
        return ToolOutput::error("missing 'path' argument");
    };
    let Some(old_text) = args.get("old_text").and_then(|o| o.as_str()) else {
        return ToolOutput::error("missing 'old_text' argument");
    };
    let Some(new_text) = args.get("new_text").and_then(|n| n.as_str()) else {
        return ToolOutput::error("missing 'new_text' argument");
    };

    let path = resolve_path(cwd, path_str);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to read {}: {}", path.display(), e)),
    };

    let count = content.matches(old_text).count();
    if count == 0 {
        return ToolOutput::error(format!(
            "old_text not found in {}. The text must match exactly including whitespace.",
            path.display()
        ));
    }
    if count > 1 {
        return ToolOutput::error(format!(
            "old_text found {} times in {}. It must be unique. Add more context to disambiguate.",
            count,
            path.display()
        ));
    }

    let new_content = content.replacen(old_text, new_text, 1);
    match std::fs::write(&path, &new_content) {
        Ok(()) => ToolOutput::text(format!("Successfully edited {}", path.display())),
        Err(e) => ToolOutput::error(format!("failed to write {}: {}", path.display(), e)),
    }
}
