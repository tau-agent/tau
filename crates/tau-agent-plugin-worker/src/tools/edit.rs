//! Edit tool — surgical find-and-replace in files.
//!
//! Supports single edit (old_text/new_text) and multi-edit (edits array).

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "edit".into(),
            description:
                "Edit a file by replacing exact text. Each old_text must match exactly (including whitespace and newlines) and be unique in the file. Supports single edit or multiple edits in one call."
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
                        "description": "Exact text to find and replace (for single edit)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "New text to replace the old text with (for single edit)"
                    },
                    "edits": {
                        "type": "array",
                        "description": "Multiple edits to apply in order (alternative to old_text/new_text)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to find"
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement text"
                                }
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                },
                "required": ["path"]
            }),
        },
        execute: Box::new(execute),
    }
}

struct Edit {
    old_text: String,
    new_text: String,
}

fn execute(args: serde_json::Value, cwd: &str) -> ToolOutput {
    let Some(path_str) = args.get("path").and_then(|p| p.as_str()) else {
        return ToolOutput::error("missing 'path' argument");
    };

    // Parse edits: either single (old_text/new_text) or multi (edits array), not both
    let has_single = args.get("old_text").is_some();
    let has_multi = args.get("edits").is_some();
    if has_single && has_multi {
        return ToolOutput::error("use either old_text/new_text or edits array, not both");
    }

    let edits = if let Some(edits_arr) = args.get("edits").and_then(|e| e.as_array()) {
        let mut parsed = Vec::new();
        for (i, edit) in edits_arr.iter().enumerate() {
            let Some(old_text) = edit.get("old_text").and_then(|o| o.as_str()) else {
                return ToolOutput::error(format!("edit[{}]: missing 'old_text'", i));
            };
            let Some(new_text) = edit.get("new_text").and_then(|n| n.as_str()) else {
                return ToolOutput::error(format!("edit[{}]: missing 'new_text'", i));
            };
            parsed.push(Edit {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            });
        }
        if parsed.is_empty() {
            return ToolOutput::error("'edits' array is empty");
        }
        parsed
    } else {
        // Single edit mode
        let Some(old_text) = args.get("old_text").and_then(|o| o.as_str()) else {
            return ToolOutput::error("missing 'old_text' (or 'edits' array)");
        };
        let Some(new_text) = args.get("new_text").and_then(|n| n.as_str()) else {
            return ToolOutput::error("missing 'new_text'");
        };
        vec![Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }]
    };

    let path = super::resolve_path(cwd, path_str);
    let mut content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to read {}: {}", path.display(), e)),
    };

    // Validate all edits before applying any
    let mut errors = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let count = content.matches(&edit.old_text[..]).count();
        let label = if edits.len() == 1 {
            String::new()
        } else {
            format!("edit[{}]: ", i)
        };
        if count == 0 {
            errors.push(format!("{}old_text not found in {}", label, path.display()));
        } else if count > 1 {
            errors.push(format!(
                "{}old_text found {} times in {}. Must be unique.",
                label,
                count,
                path.display()
            ));
        }
    }
    if !errors.is_empty() {
        return ToolOutput::error(errors.join("\n"));
    }

    // Apply edits in order
    for edit in &edits {
        content = content.replacen(&edit.old_text, &edit.new_text, 1);
    }

    match std::fs::write(&path, &content) {
        Ok(()) => {
            let n_edits = edits.len();
            let (added, removed): (usize, usize) = edits.iter().fold((0, 0), |(a, r), edit| {
                (
                    a + if edit.new_text.is_empty() {
                        0
                    } else {
                        edit.new_text.lines().count().max(1)
                    },
                    r + if edit.old_text.is_empty() {
                        0
                    } else {
                        edit.old_text.lines().count().max(1)
                    },
                )
            });
            let summary = if n_edits == 1 {
                format!("edit: {} (+{} -{} lines)", path_str, added, removed)
            } else {
                format!(
                    "edit: {} (+{} -{} lines, {} edits)",
                    path_str, added, removed, n_edits
                )
            };
            if edits.len() == 1 {
                ToolOutput::text(format!("Successfully edited {}", path.display()))
                    .with_summary(summary)
            } else {
                ToolOutput::text(format!(
                    "Successfully applied {} edits to {}",
                    edits.len(),
                    path.display()
                ))
                .with_summary(summary)
            }
        }
        Err(e) => ToolOutput::error(format!("failed to write {}: {}", path.display(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_file(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn single_edit() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "old_text": "hello",
                "new_text": "goodbye"
            }),
            "/tmp",
        );
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[test]
    fn multi_edit() {
        let (_dir, path) = setup_file("aaa bbb ccc");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "edits": [
                    {"old_text": "aaa", "new_text": "AAA"},
                    {"old_text": "ccc", "new_text": "CCC"}
                ]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "AAA bbb CCC");
    }

    #[test]
    fn multi_edit_validation_fails_all_before_apply() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "edits": [
                    {"old_text": "hello", "new_text": "goodbye"},
                    {"old_text": "NOTFOUND", "new_text": "xxx"}
                ]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        // File should be unchanged — validation failed before any edits applied
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn edit_not_found() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "old_text": "NOTFOUND",
                "new_text": "xxx"
            }),
            "/tmp",
        );
        assert!(result.is_error);
    }

    #[test]
    fn edit_ambiguous() {
        let (_dir, path) = setup_file("aaa aaa bbb");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "old_text": "aaa",
                "new_text": "xxx"
            }),
            "/tmp",
        );
        assert!(result.is_error);
        // File should be unchanged
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa aaa bbb");
    }
}
