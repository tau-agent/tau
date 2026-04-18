//! Edit tool — surgical find-and-replace in files.
//!
//! Input shape is `edits: [{old_text, new_text}, ...]`. Each entry is applied
//! in order; validation (uniqueness of `old_text`, presence in the file) runs
//! for every entry before any mutation, so a failure leaves the file
//! untouched.
//!
//! A pre-validation [`prepare_arguments`] hook (see
//! [`super::ToolDef::prepare_arguments`]) silently folds the legacy top-level
//! `{old_text, new_text}` shape into `{edits: [{old_text, new_text}]}`. This
//! lets resumed sessions whose history predates the edits-only schema keep
//! working without polluting the public tool schema shown to the model.

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "edit".into(),
            description:
                "Edit a file by replacing exact text. Each old_text must match exactly (including whitespace and newlines) and be unique in the file. Supports single edit or multiple disjoint edits in one call."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more edits to apply in order. Each edit's old_text must be unique in the file.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to find (must be unique in the file)"
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
                "required": ["path", "edits"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: Some(Box::new(prepare_arguments)),
    }
}

struct Edit {
    old_text: String,
    new_text: String,
}

/// Fold legacy top-level `{old_text, new_text}` tool-call arguments into the
/// edits-only schema. Used as the `prepare_arguments` hook so resumed sessions
/// that recorded the old shape in their history validate cleanly.
///
/// If the input isn't an object, or doesn't carry string `old_text` +
/// `new_text` top-level fields, it is returned unchanged.
fn prepare_arguments(mut args: serde_json::Value) -> serde_json::Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };

    let has_legacy = obj.get("old_text").map(|v| v.is_string()).unwrap_or(false)
        && obj.get("new_text").map(|v| v.is_string()).unwrap_or(false);
    if !has_legacy {
        return args;
    }

    let Some(old_text) = obj.remove("old_text") else {
        return args;
    };
    let Some(new_text) = obj.remove("new_text") else {
        return args;
    };

    let legacy_edit = serde_json::json!({
        "old_text": old_text,
        "new_text": new_text,
    });

    match obj.get_mut("edits").and_then(|e| e.as_array_mut()) {
        Some(arr) => arr.push(legacy_edit),
        None => {
            obj.insert(
                "edits".to_string(),
                serde_json::Value::Array(vec![legacy_edit]),
            );
        }
    }

    // Folded legacy top-level old_text/new_text into edits[]. This is
    // expected on session resume; no user-visible signal needed.
    args
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    _cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    let Some(path_str) = args.get("path").and_then(|p| p.as_str()) else {
        return ToolOutput::error("missing 'path' argument");
    };

    let Some(edits_arr) = args.get("edits").and_then(|e| e.as_array()) else {
        return ToolOutput::error("missing 'edits' array (expected [{ old_text, new_text }, ...])");
    };
    if edits_arr.is_empty() {
        return ToolOutput::error("'edits' array is empty — provide at least one edit");
    }

    let mut edits = Vec::with_capacity(edits_arr.len());
    for (i, edit) in edits_arr.iter().enumerate() {
        let Some(old_text) = edit.get("old_text").and_then(|o| o.as_str()) else {
            return ToolOutput::error(format!("edits[{}]: missing 'old_text'", i));
        };
        let Some(new_text) = edit.get("new_text").and_then(|n| n.as_str()) else {
            return ToolOutput::error(format!("edits[{}]: missing 'new_text'", i));
        };
        edits.push(Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }

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
            format!("edits[{}]: ", i)
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
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("test.txt");
        std::fs::write(&path, content).expect("write test file");
        (dir, path)
    }

    #[test]
    fn single_edit() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "hello", "new_text": "goodbye"}]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "goodbye world"
        );
    }

    #[test]
    fn multi_edit() {
        let (_dir, path) = setup_file("aaa bbb ccc");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [
                    {"old_text": "aaa", "new_text": "AAA"},
                    {"old_text": "ccc", "new_text": "CCC"}
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "AAA bbb CCC"
        );
    }

    #[test]
    fn multi_edit_validation_fails_all_before_apply() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [
                    {"old_text": "hello", "new_text": "goodbye"},
                    {"old_text": "NOTFOUND", "new_text": "xxx"}
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        // File should be unchanged — validation failed before any edits applied
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "hello world"
        );
    }

    #[test]
    fn edit_not_found() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "NOTFOUND", "new_text": "xxx"}]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
    }

    #[test]
    fn edit_ambiguous() {
        let (_dir, path) = setup_file("aaa aaa bbb");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "aaa", "new_text": "xxx"}]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        // File should be unchanged
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "aaa aaa bbb"
        );
    }

    #[test]
    fn empty_edits_array_rejected() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": []
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(
            msg.contains("empty") || msg.contains("at least one"),
            "error message should mention empty/at-least-one, got: {msg}"
        );
    }

    #[test]
    fn missing_edits_rejected() {
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("edits"));
    }

    // --- prepare_arguments (legacy input fold) ---------------------------

    #[test]
    fn prepare_arguments_folds_legacy_top_level_fields() {
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "old_text": "foo",
            "new_text": "bar",
        });
        let prepared = prepare_arguments(input);
        assert_eq!(
            prepared,
            serde_json::json!({
                "path": "/tmp/x.txt",
                "edits": [{"old_text": "foo", "new_text": "bar"}],
            })
        );
    }

    #[test]
    fn prepare_arguments_passthrough_when_no_legacy_fields() {
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "edits": [{"old_text": "a", "new_text": "b"}],
        });
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_appends_legacy_to_existing_edits() {
        // Defensive: if a caller somehow provides BOTH edits[] and legacy
        // old_text/new_text, fold the legacy pair onto the end rather than
        // dropping either shape.
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "edits": [{"old_text": "a", "new_text": "b"}],
            "old_text": "c",
            "new_text": "d",
        });
        let prepared = prepare_arguments(input);
        assert_eq!(
            prepared,
            serde_json::json!({
                "path": "/tmp/x.txt",
                "edits": [
                    {"old_text": "a", "new_text": "b"},
                    {"old_text": "c", "new_text": "d"},
                ],
            })
        );
    }

    #[test]
    fn prepare_arguments_ignores_non_object_input() {
        let input = serde_json::json!("not an object");
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_ignores_non_string_legacy_fields() {
        // If old_text / new_text are not strings, don't touch the input —
        // let downstream validation produce the error.
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "old_text": 42,
            "new_text": "bar",
        });
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn legacy_input_end_to_end_via_execute_tool() {
        // Simulate a resumed session: tool_call.arguments carries the old
        // top-level shape. execute_tool must fold it and the edit must succeed.
        use super::super::{ToolDef, execute_tool};
        use tau_agent_plugin::ToolCall;

        let (_dir, path) = setup_file("hello world");
        let tools: Vec<ToolDef> = vec![tool_def()];
        let tool_call = ToolCall {
            id: "call_1".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "old_text": "hello",
                "new_text": "goodbye",
            }),
        };
        let result = execute_tool(
            &tools,
            &tool_call,
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "goodbye world"
        );
    }

    #[test]
    fn legacy_input_with_relative_path_resolves_against_cwd() {
        // Legacy top-level-keys shape + a relative `path` — the prepare hook
        // folds the edit, then resolve_path anchors it at cwd.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested.txt");
        std::fs::write(&path, "alpha beta").expect("write");

        use super::super::{ToolDef, execute_tool};
        use tau_agent_plugin::ToolCall;
        let tools: Vec<ToolDef> = vec![tool_def()];
        let tool_call = ToolCall {
            id: "call_2".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": "nested.txt",
                "old_text": "alpha",
                "new_text": "ALPHA",
            }),
        };
        let result = execute_tool(
            &tools,
            &tool_call,
            dir.path().to_str().expect("cwd"),
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "ALPHA beta"
        );
    }
}
