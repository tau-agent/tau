//! Edit tool — surgical find-and-replace across one or more files.
//!
//! Canonical input shape is `files: [{path, edits: [{old_text, new_text}, ...]}, ...]`.
//! All edits across all files are validated up front (file readability,
//! `old_text` presence and uniqueness, no duplicate paths within a single
//! call). If any check fails, no file is mutated.
//!
//! Two legacy/compat shapes are folded into the canonical shape by
//! [`prepare_arguments`] (see [`super::ToolDef::prepare_arguments`]):
//!
//! 1. `{path, old_text, new_text}` — the original single-edit shape, used by
//!    very old resumed sessions.
//! 2. `{path, edits: [...]}` — the single-file multi-edit shape that
//!    preceded multi-file batching.
//!
//! Both fold transparently into `files: [{path, edits: [...]}]` so `execute`
//! only ever reads `files`. The transformed args are only fed to `execute`;
//! the recorded `tool_call.arguments` keep whatever shape the model sent, so
//! TUI / log consumers see the original input verbatim.
//!
//! ### Failure mode: mid-batch disk writes
//!
//! Phase ordering is validate-everything → write-everything. Validation makes
//! every logical failure happen before any byte hits disk. The one tolerated
//! failure mode is a disk error (out-of-space, permission flip mid-call,
//! …) partway through the write phase: earlier files are already on disk,
//! later files are not. This matches what would happen if the model issued
//! N sequential single-file edit calls and the box ran out of disk halfway
//! through.

use super::{ToolDef, ToolOutput};
use std::path::PathBuf;
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "edit".into(),
            description:
                "Edit a file by replacing exact text. Each old_text must match exactly (including whitespace and newlines) and be unique in the file. Supports single edit or multiple disjoint edits in one call, including multiple disjoint edits in one call across one or more files."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit (single-file form)"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more edits to apply in order. Each edit's old_text must be unique in the file. Single-file form: pair with `path`.",
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
                    },
                    "files": {
                        "type": "array",
                        "description": "Multi-file form: one entry per file, each with its own edits[]. All edits across all files are validated before any file is written.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Path to the file to edit"
                                },
                                "edits": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "old_text": { "type": "string" },
                                            "new_text": { "type": "string" }
                                        },
                                        "required": ["old_text", "new_text"]
                                    }
                                }
                            },
                            "required": ["path", "edits"]
                        }
                    }
                },
                "oneOf": [
                    { "required": ["files"] },
                    { "required": ["path", "edits"] }
                ]
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

struct FileEdits {
    /// The path string as the model sent it (used in summaries / error
    /// messages so the model sees its own paths, not their resolved form).
    path_str: String,
    /// Resolved absolute path used for read/write and duplicate detection.
    resolved: PathBuf,
    edits: Vec<Edit>,
}

/// Fold legacy and single-file argument shapes into the canonical
/// `files: [{path, edits}]` form.
///
/// Transformation order (each step is a no-op when its trigger is absent):
///
/// 1. Top-level `{old_text, new_text}` strings → append to / create top-level
///    `edits[]`. (Original legacy fold.)
/// 2. After step 1, if `files` is absent and we have top-level `path` +
///    `edits`, wrap them into `files: [{path, edits}]` and remove the
///    top-level `path` / `edits` keys.
/// 3. If `files` is already present, leave it untouched. (Stray top-level
///    `path` / `edits` alongside `files` is malformed input — the validator
///    in `execute` rejects it via the absence of canonical `files` not
///    matching, but in practice `oneOf` already forbids it.)
fn prepare_arguments(mut args: serde_json::Value) -> serde_json::Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };

    // Step 1: legacy {old_text, new_text} → edits[]
    let has_legacy = obj.get("old_text").map(|v| v.is_string()).unwrap_or(false)
        && obj.get("new_text").map(|v| v.is_string()).unwrap_or(false);
    if has_legacy {
        if let (Some(old_text), Some(new_text)) = (obj.remove("old_text"), obj.remove("new_text")) {
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
        }
    }

    // Step 2: single-file {path, edits} → {files: [{path, edits}]}
    // Skip if `files` is already present; in that case the existing
    // `files` wins and any stray top-level `path`/`edits` is left alone for
    // the validator to flag as malformed input (or oneOf to reject).
    if !obj.contains_key("files")
        && obj.get("path").map(|v| v.is_string()).unwrap_or(false)
        && obj.get("edits").map(|v| v.is_array()).unwrap_or(false)
    {
        if let (Some(path), Some(edits)) = (obj.remove("path"), obj.remove("edits")) {
            let entry = serde_json::json!({
                "path": path,
                "edits": edits,
            });
            obj.insert("files".to_string(), serde_json::Value::Array(vec![entry]));
        }
    }

    args
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    _cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    let Some(files_arr) = args.get("files").and_then(|f| f.as_array()) else {
        return ToolOutput::error(
            "missing 'files' array (expected [{ path, edits: [{ old_text, new_text }, ...] }, ...])",
        );
    };
    if files_arr.is_empty() {
        return ToolOutput::error("'files' array is empty — provide at least one file");
    }

    // ---- Phase 1: parse ----
    let mut files: Vec<FileEdits> = Vec::with_capacity(files_arr.len());
    for (fi, file) in files_arr.iter().enumerate() {
        let Some(file_obj) = file.as_object() else {
            return ToolOutput::error(format!("files[{}]: expected object", fi));
        };
        let Some(path_str) = file_obj.get("path").and_then(|p| p.as_str()) else {
            return ToolOutput::error(format!("files[{}]: missing 'path'", fi));
        };
        let Some(edits_arr) = file_obj.get("edits").and_then(|e| e.as_array()) else {
            return ToolOutput::error(format!(
                "files[{}]: missing 'edits' array (expected [{{ old_text, new_text }}, ...])",
                fi
            ));
        };
        if edits_arr.is_empty() {
            return ToolOutput::error(format!(
                "files[{}]: 'edits' array is empty — provide at least one edit",
                fi
            ));
        }

        let mut edits = Vec::with_capacity(edits_arr.len());
        for (ei, edit) in edits_arr.iter().enumerate() {
            let Some(old_text) = edit.get("old_text").and_then(|o| o.as_str()) else {
                return ToolOutput::error(format!(
                    "files[{}].edits[{}]: missing 'old_text'",
                    fi, ei
                ));
            };
            let Some(new_text) = edit.get("new_text").and_then(|n| n.as_str()) else {
                return ToolOutput::error(format!(
                    "files[{}].edits[{}]: missing 'new_text'",
                    fi, ei
                ));
            };
            edits.push(Edit {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            });
        }

        let resolved = super::resolve_path(cwd, path_str);
        files.push(FileEdits {
            path_str: path_str.to_string(),
            resolved,
            edits,
        });
    }

    // ---- Phase 2: reject duplicate paths within the call ----
    for i in 0..files.len() {
        for j in (i + 1)..files.len() {
            if files[i].resolved == files[j].resolved {
                return ToolOutput::error(format!(
                    "files[{}].path duplicates files[{}].path ({})",
                    j,
                    i,
                    files[i].resolved.display()
                ));
            }
        }
    }

    // ---- Phase 3: read every file ----
    // We hold the original content per file for validation and use a working
    // copy during apply.
    let mut originals: Vec<String> = Vec::with_capacity(files.len());
    for (fi, f) in files.iter().enumerate() {
        match std::fs::read_to_string(&f.resolved) {
            Ok(c) => originals.push(c),
            Err(e) => {
                return ToolOutput::error(format!(
                    "files[{}]: failed to read {}: {}",
                    fi,
                    f.resolved.display(),
                    e
                ));
            }
        }
    }

    // ---- Phase 4: validate every edit, aggregating errors ----
    // Per-file: validate each edit's `old_text` is unique in that file's
    // *original* content. Edits within a single file are required to be
    // disjoint by the existing single-file contract (documented in the
    // edit guidelines), so checking against the original is sound and
    // matches today's single-file behaviour.
    let mut errors: Vec<String> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let content = &originals[fi];
        for (ei, edit) in f.edits.iter().enumerate() {
            let count = content.matches(&edit.old_text[..]).count();
            // For single-file single-edit calls keep the legacy unlabelled
            // error message ("old_text not found in /path") so error-string
            // consumers (logs, tests downstream) don't see a regression.
            let label = if files.len() == 1 && f.edits.len() == 1 {
                String::new()
            } else if files.len() == 1 {
                format!("edits[{}]: ", ei)
            } else {
                format!("files[{}].edits[{}]: ", fi, ei)
            };
            if count == 0 {
                errors.push(format!(
                    "{}old_text not found in {}",
                    label,
                    f.resolved.display()
                ));
            } else if count > 1 {
                errors.push(format!(
                    "{}old_text found {} times in {}. Must be unique.",
                    label,
                    count,
                    f.resolved.display()
                ));
            }
        }
    }
    if !errors.is_empty() {
        return ToolOutput::error(errors.join("\n"));
    }

    // ---- Phase 5: apply ----
    // Run all edits per file against a working copy of the original, then
    // write. Writes happen in input order; a disk error mid-batch leaves
    // earlier files written and later ones not — see module docs.
    let mut updated: Vec<String> = Vec::with_capacity(files.len());
    for (fi, f) in files.iter().enumerate() {
        let mut content = originals[fi].clone();
        for edit in &f.edits {
            content = content.replacen(&edit.old_text, &edit.new_text, 1);
        }
        updated.push(content);
    }

    for (fi, f) in files.iter().enumerate() {
        if let Err(e) = std::fs::write(&f.resolved, &updated[fi]) {
            return ToolOutput::error(format!(
                "files[{}]: failed to write {}: {}",
                fi,
                f.resolved.display(),
                e
            ));
        }
    }

    // ---- Phase 6: summarise ----
    let n_files = files.len();
    let total_edits: usize = files.iter().map(|f| f.edits.len()).sum();
    let (total_added, total_removed): (usize, usize) = files.iter().fold((0, 0), |(a, r), f| {
        f.edits.iter().fold((a, r), |(a, r), edit| {
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
        })
    });

    if n_files == 1 {
        // Preserve byte-for-byte the existing single-file summary format so
        // TUI / log consumers don't see a regression.
        let f = &files[0];
        let summary = if f.edits.len() == 1 {
            format!(
                "edit: {} (+{} -{} lines)",
                f.path_str, total_added, total_removed
            )
        } else {
            format!(
                "edit: {} (+{} -{} lines, {} edits)",
                f.path_str,
                total_added,
                total_removed,
                f.edits.len()
            )
        };
        let body = if f.edits.len() == 1 {
            format!("Successfully edited {}", f.resolved.display())
        } else {
            format!(
                "Successfully applied {} edits to {}",
                f.edits.len(),
                f.resolved.display()
            )
        };
        ToolOutput::text(body).with_summary(summary)
    } else {
        let summary = format!(
            "edit: {} files (+{} -{} lines, {} edits)",
            n_files, total_added, total_removed, total_edits
        );
        // Body: list paths, truncating past 5 to "… and N more".
        const MAX_LISTED: usize = 5;
        let listed: Vec<&str> = files
            .iter()
            .take(MAX_LISTED)
            .map(|f| f.path_str.as_str())
            .collect();
        let suffix = if n_files > MAX_LISTED {
            format!(", … and {} more", n_files - MAX_LISTED)
        } else {
            String::new()
        };
        let body = format!(
            "Successfully applied {} edits across {} files: {}{}",
            total_edits,
            n_files,
            listed.join(", "),
            suffix
        );
        ToolOutput::text(body).with_summary(summary)
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
    fn single_edit_canonical_files_shape() {
        // execute() requires the canonical `files` shape; the legacy /
        // single-file shapes go through prepare_arguments first (see
        // single_file_form_via_execute_tool below).
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "files": [{
                    "path": path.to_str().expect("path to str"),
                    "edits": [{"old_text": "hello", "new_text": "goodbye"}]
                }]
            }),
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
    fn execute_rejects_non_canonical_shape() {
        // Direct execute() with the single-file shape is rejected — the
        // canonical fold lives in prepare_arguments.
        let (_dir, path) = setup_file("hello world");
        let result = execute(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "hello", "new_text": "goodbye"}]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "hello world"
        );
    }

    /// Run a tool call end-to-end through `execute_tool` so
    /// `prepare_arguments` runs first. Most tests prefer this over calling
    /// `execute` directly because it exercises the public contract (the
    /// shape the model sends).
    fn run_tool(args: serde_json::Value, cwd: &str) -> super::super::ToolResultMessage {
        use super::super::execute_tool;
        use tau_agent_plugin::ToolCall;
        let tools = vec![tool_def()];
        let tool_call = ToolCall {
            id: "call_test".into(),
            name: "edit".into(),
            arguments: args,
        };
        execute_tool(
            &tools,
            &tool_call,
            cwd,
            &tau_agent_plugin::CancelToken::new(),
        )
    }

    #[test]
    fn single_file_form_via_execute_tool() {
        let (_dir, path) = setup_file("hello world");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "hello", "new_text": "goodbye"}]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "goodbye world"
        );
    }

    #[test]
    fn multi_edit() {
        let (_dir, path) = setup_file("aaa bbb ccc");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [
                    {"old_text": "aaa", "new_text": "AAA"},
                    {"old_text": "ccc", "new_text": "CCC"}
                ]
            }),
            "/tmp",
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
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [
                    {"old_text": "hello", "new_text": "goodbye"},
                    {"old_text": "NOTFOUND", "new_text": "xxx"}
                ]
            }),
            "/tmp",
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
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "NOTFOUND", "new_text": "xxx"}]
            }),
            "/tmp",
        );
        assert!(result.is_error);
    }

    #[test]
    fn edit_ambiguous() {
        let (_dir, path) = setup_file("aaa aaa bbb");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "aaa", "new_text": "xxx"}]
            }),
            "/tmp",
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
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": []
            }),
            "/tmp",
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
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
            }),
            "/tmp",
        );
        assert!(result.is_error);
        // No `edits` and no `files` — execute reports the canonical 'files' miss.
        assert!(result.content[0].text().contains("files"));
    }

    // --- multi-file behaviour --------------------------------------------

    #[test]
    fn multi_file_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "alpha one").expect("write");
        std::fs::write(&b, "beta two").expect("write");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": a.to_str().expect("path"), "edits": [{"old_text": "alpha", "new_text": "ALPHA"}]},
                    {"path": b.to_str().expect("path"), "edits": [{"old_text": "beta", "new_text": "BETA"}]}
                ]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(std::fs::read_to_string(&a).expect("read a"), "ALPHA one");
        assert_eq!(std::fs::read_to_string(&b).expect("read b"), "BETA two");
        let summary = result.summary.expect("summary present");
        assert!(
            summary.contains("2 files") && summary.contains("2 edits"),
            "summary: {summary}"
        );
    }

    #[test]
    fn multi_file_partial_failure_no_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        std::fs::write(&a, "alpha").expect("write");
        std::fs::write(&b, "beta").expect("write");
        std::fs::write(&c, "gamma").expect("write");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": a.to_str().expect("path"), "edits": [{"old_text": "alpha", "new_text": "ALPHA"}]},
                    {"path": b.to_str().expect("path"), "edits": [{"old_text": "NOTFOUND", "new_text": "xxx"}]},
                    {"path": c.to_str().expect("path"), "edits": [{"old_text": "gamma", "new_text": "GAMMA"}]}
                ]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(msg.contains("files[1]"), "error refs files[1]: {msg}");
        // All three files unchanged.
        assert_eq!(std::fs::read_to_string(&a).expect("read a"), "alpha");
        assert_eq!(std::fs::read_to_string(&b).expect("read b"), "beta");
        assert_eq!(std::fs::read_to_string(&c).expect("read c"), "gamma");
    }

    #[test]
    fn multi_file_read_failure_no_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let missing = dir.path().join("does_not_exist.txt");
        std::fs::write(&a, "alpha").expect("write");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": a.to_str().expect("path"), "edits": [{"old_text": "alpha", "new_text": "ALPHA"}]},
                    {"path": missing.to_str().expect("path"), "edits": [{"old_text": "x", "new_text": "y"}]}
                ]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        // The valid sibling must not have been written.
        assert_eq!(std::fs::read_to_string(&a).expect("read a"), "alpha");
    }

    #[test]
    fn multi_file_duplicate_path_rejected() {
        let (_dir, path) = setup_file("alpha beta");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": path.to_str().expect("path"), "edits": [{"old_text": "alpha", "new_text": "ALPHA"}]},
                    {"path": path.to_str().expect("path"), "edits": [{"old_text": "beta", "new_text": "BETA"}]}
                ]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(msg.contains("duplicate"), "error: {msg}");
        // File unchanged.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "alpha beta");
    }

    #[test]
    fn multi_file_summary_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "alpha").expect("write");
        std::fs::write(&b, "beta").expect("write");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": a.to_str().expect("path"), "edits": [{"old_text": "alpha", "new_text": "A"}]},
                    {"path": b.to_str().expect("path"), "edits": [{"old_text": "beta", "new_text": "B"}]}
                ]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        let summary = result.summary.expect("summary present");
        assert_eq!(
            summary, "edit: 2 files (+2 -2 lines, 2 edits)",
            "got summary: {summary}"
        );
    }

    #[test]
    fn single_file_summary_format_unchanged() {
        // Regression guard: existing TUI / log consumers expect this exact
        // format for the single-file single-edit common case.
        let (_dir, path) = setup_file("hello");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "hello", "new_text": "world"}]
            }),
            "/tmp",
        );
        assert!(!result.is_error);
        let summary = result.summary.expect("summary present");
        let path_str = path.to_str().expect("path");
        assert_eq!(summary, format!("edit: {} (+1 -1 lines)", path_str));
    }

    // --- prepare_arguments folds ----------------------------------------

    #[test]
    fn prepare_arguments_folds_legacy_top_level_fields() {
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "old_text": "foo",
            "new_text": "bar",
        });
        let prepared = prepare_arguments(input);
        // Legacy → edits[] → wrap into files[].
        assert_eq!(
            prepared,
            serde_json::json!({
                "files": [{
                    "path": "/tmp/x.txt",
                    "edits": [{"old_text": "foo", "new_text": "bar"}],
                }],
            })
        );
    }

    #[test]
    fn prepare_arguments_wraps_single_file_into_files() {
        let input = serde_json::json!({
            "path": "/tmp/x.txt",
            "edits": [{"old_text": "a", "new_text": "b"}],
        });
        let prepared = prepare_arguments(input);
        assert_eq!(
            prepared,
            serde_json::json!({
                "files": [{
                    "path": "/tmp/x.txt",
                    "edits": [{"old_text": "a", "new_text": "b"}],
                }],
            })
        );
    }

    #[test]
    fn prepare_arguments_passthrough_when_files_present() {
        let input = serde_json::json!({
            "files": [{
                "path": "/tmp/x.txt",
                "edits": [{"old_text": "a", "new_text": "b"}],
            }],
        });
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_appends_legacy_to_existing_edits_then_wraps() {
        // Defensive: if a caller somehow provides BOTH edits[] and legacy
        // old_text/new_text, fold the legacy pair onto the end and then wrap
        // the whole thing into the canonical files[] form.
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
                "files": [{
                    "path": "/tmp/x.txt",
                    "edits": [
                        {"old_text": "a", "new_text": "b"},
                        {"old_text": "c", "new_text": "d"},
                    ],
                }],
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
        // If old_text / new_text are not strings, don't touch the legacy
        // fields. There's no `edits` and no `files` either, so the result
        // is the input verbatim — downstream validation produces the error.
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
        let (_dir, path) = setup_file("hello world");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "old_text": "hello",
                "new_text": "goodbye",
            }),
            "/tmp",
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
        let result = run_tool(
            serde_json::json!({
                "path": "nested.txt",
                "old_text": "alpha",
                "new_text": "ALPHA",
            }),
            dir.path().to_str().expect("cwd"),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "ALPHA beta"
        );
    }
}
