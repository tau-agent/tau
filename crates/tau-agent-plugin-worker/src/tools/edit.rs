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

use super::line_hash::{extract_anchor_token, hash_lines_with_disambiguators};
use super::{ToolDef, ToolOutput};
use std::path::PathBuf;
use tau_agent_plugin::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "edit".into(),
            description:
                "Edit a file by replacing exact text or by line-anchor. Each old_text must match exactly (including whitespace and newlines) and be unique in the file. Anchor edits use the `<hash>§` prefixes from the `read` tool's output and re-validate against current file contents. Supports single edit or multiple disjoint edits in one call across one or more files."
                    .into(),
            // The schema accepts two mutually exclusive shapes — single-file
            // (`path` + `edits`) and multi-file (`files`) — but does NOT
            // express that mutual exclusion structurally. Anthropic's
            // tool-input-schema validator rejects top-level
            // `oneOf`/`anyOf`/`allOf` with HTTP 400, so the constraint lives
            // in the descriptions and is enforced at runtime by
            // `prepare_arguments` and `execute` instead.
            parameters: serde_json::json!({
                "type": "object",
                "description": "Provide EITHER `files` (multi-file form) OR `path` + `edits` (single-file form). Do not mix the two — supplying both, or neither, is an error.",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Single-file form: path to the file to edit. Pair with `edits`. Mutually exclusive with `files`."
                    },
                    "edits": {
                        "type": "array",
                        "description": "Single-file form: one or more edits to apply in order to `path`. Each edit must be either a legacy `{old_text, new_text}` (old_text unique in the file) or an anchor edit `{edit_type, anchor, end_anchor?, text}` (anchor copied from the read tool's `<hash>§` prefix). Edits must not overlap. Mutually exclusive with `files`.",
                        "minItems": 1,
                        "items": EDIT_ITEM_SCHEMA.clone()
                    },
                    "files": {
                        "type": "array",
                        "description": "Multi-file form: one entry per file, each with its own edits[]. All edits across all files are validated before any file is written. Mutually exclusive with top-level `path` / `edits`.",
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
                                    "items": EDIT_ITEM_SCHEMA.clone()
                                }
                            },
                            "required": ["path", "edits"]
                        }
                    }
                }
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: Some(Box::new(prepare_arguments)),
    }
}

static EDIT_ITEM_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "description": "Either a legacy text edit `{old_text, new_text}` or an anchor edit `{edit_type, anchor, end_anchor?, text}`.",
        "properties": {
            "old_text": {
                "type": "string",
                "description": "Legacy form: exact text to find (must be unique in the file)."
            },
            "new_text": {
                "type": "string",
                "description": "Legacy form: replacement text."
            },
            "edit_type": {
                "type": "string",
                "enum": ["replace", "insert_before", "insert_after"],
                "description": "Anchor form: kind of edit. `replace` is inclusive on both ends."
            },
            "anchor": {
                "type": "string",
                "description": "Anchor form: line anchor (the `<hash>` or `<hash>.<n>` token from the read tool's prefix). The full hashed line `<hash>§<line>` is also accepted — everything from the first § onward is ignored."
            },
            "end_anchor": {
                "type": "string",
                "description": "Anchor form (replace only): end anchor for a multi-line range. Defaults to `anchor` when omitted (single-line replace). Inclusive."
            },
            "text": {
                "type": "string",
                "description": "Anchor form: text to insert or substitute. Multi-line text is supported as-is."
            }
        }
    })
});

/// One edit, in either of the two accepted shapes.
enum Edit {
    /// Legacy `{old_text, new_text}` find-and-replace.
    Legacy { old_text: String, new_text: String },
    /// New anchor-based edit.
    Anchor {
        kind: AnchorKind,
        anchor: String,
        /// Only meaningful for [`AnchorKind::Replace`]; defaults to `anchor`
        /// when the model omitted it.
        end_anchor: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum AnchorKind {
    Replace,
    InsertBefore,
    InsertAfter,
}

struct FileEdits {
    /// The path string as the model sent it (used in summaries / error
    /// messages so the model sees its own paths, not their resolved form).
    path_str: String,
    /// Resolved absolute path used for read/write and duplicate detection.
    resolved: PathBuf,
    edits: Vec<Edit>,
}

/// A single edit's resolved line range on the original file. Both ends are
/// inclusive 0-based line indices. For an insertion edit, both ends point
/// at the anchor line itself — we use this to detect insertions colliding
/// with each other or with a replace.
struct EditPlan {
    edit_index: usize,
    line_start: usize,
    line_end: usize,
}

/// Convert a `[byte_start, byte_end)` span on `content` to an inclusive
/// `(line_start, line_end)` 0-based line range. Used to plot legacy
/// `old_text` matches onto the per-file line grid for overlap detection.
fn byte_span_to_line_span(content: &str, byte_start: usize, byte_end: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut line_start = 0usize;
    let mut at_byte = 0usize;
    let mut found_start = false;
    for ch in content.chars() {
        if !found_start && at_byte >= byte_start {
            line_start = line;
            found_start = true;
        }
        // `byte_end` is exclusive; the last consumed byte is at byte_end-1.
        if at_byte >= byte_end.saturating_sub(1) {
            return (line_start, line);
        }
        if ch == '\n' {
            line += 1;
        }
        at_byte += ch.len_utf8();
    }
    if !found_start {
        line_start = line;
    }
    (line_start, line)
}

/// Per-edit (added, removed) line counts for the summary line. The plan
/// is used to recover the inclusive line range of an anchor `replace` so
/// we can report removed lines accurately.
fn edit_line_deltas(edit: &Edit, plan: &EditPlan) -> (usize, usize) {
    match edit {
        Edit::Legacy { old_text, new_text } => {
            let added = if new_text.is_empty() {
                0
            } else {
                new_text.lines().count().max(1)
            };
            let removed = if old_text.is_empty() {
                0
            } else {
                old_text.lines().count().max(1)
            };
            (added, removed)
        }
        Edit::Anchor { kind, text, .. } => {
            let text_lines = if text.is_empty() {
                0
            } else {
                text.split('\n').count()
            };
            match kind {
                AnchorKind::Replace => {
                    let removed = plan.line_end - plan.line_start + 1;
                    (text_lines, removed)
                }
                AnchorKind::InsertBefore | AnchorKind::InsertAfter => (text_lines, 0),
            }
        }
    }
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
///    `path` / `edits` alongside `files` is malformed input — `execute`
///    rejects it with a mixed-shapes error, since the schema can't encode
///    the mutual exclusion structurally.)
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
    // `execute` to flag as a mixed-shape error (the schema used to encode
    // this with a top-level `oneOf`, but Anthropic rejects that).
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
    // The schema can't enforce "exactly one of {files} or {path+edits}"
    // structurally (Anthropic rejects top-level oneOf/anyOf/allOf), so we
    // validate it here on the post-`prepare_arguments` value. After that
    // hook runs, a well-formed call has *only* `files`; any stray
    // top-level `path` or `edits` alongside `files` means the model mixed
    // the two shapes.
    let obj = args.as_object();
    let has_files = obj
        .and_then(|o| o.get("files"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let has_path = obj
        .and_then(|o| o.get("path"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let has_edits = obj
        .and_then(|o| o.get("edits"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if has_files && (has_path || has_edits) {
        return ToolOutput::error(
            "edit: provide EITHER `files` (multi-file form) OR `path` + `edits` (single-file form), not both. Top-level `path`/`edits` were sent alongside `files`.",
        );
    }
    if !has_files && !has_path && !has_edits {
        return ToolOutput::error(
            "edit: missing arguments. Provide either `files: [{path, edits: [...]}, ...]` (multi-file form) or `path` + `edits: [...]` (single-file form).",
        );
    }

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
            let edit_obj = match edit.as_object() {
                Some(o) => o,
                None => {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: expected object",
                        fi, ei
                    ));
                }
            };

            // Anchor edits are detected by the presence of `edit_type`.
            // Legacy edits use `old_text` + `new_text`. The two shapes are
            // mutually exclusive on a per-edit basis.
            if let Some(edit_type) = edit_obj.get("edit_type").and_then(|v| v.as_str()) {
                let kind = match edit_type {
                    "replace" => AnchorKind::Replace,
                    "insert_before" => AnchorKind::InsertBefore,
                    "insert_after" => AnchorKind::InsertAfter,
                    other => {
                        return ToolOutput::error(format!(
                            "files[{}].edits[{}]: unknown edit_type '{}'; expected one of replace, insert_before, insert_after",
                            fi, ei, other
                        ));
                    }
                };
                let Some(anchor_raw) = edit_obj.get("anchor").and_then(|v| v.as_str()) else {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: missing 'anchor'",
                        fi, ei
                    ));
                };
                let Some(text) = edit_obj.get("text").and_then(|v| v.as_str()) else {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: missing 'text'",
                        fi, ei
                    ));
                };
                let anchor = extract_anchor_token(anchor_raw).to_string();
                if anchor.is_empty() {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: 'anchor' is empty after stripping line content",
                        fi, ei
                    ));
                }
                let end_anchor = match edit_obj.get("end_anchor").and_then(|v| v.as_str()) {
                    Some(s) => {
                        let t = extract_anchor_token(s).to_string();
                        if t.is_empty() {
                            return ToolOutput::error(format!(
                                "files[{}].edits[{}]: 'end_anchor' is empty after stripping line content",
                                fi, ei
                            ));
                        }
                        t
                    }
                    None => anchor.clone(),
                };
                edits.push(Edit::Anchor {
                    kind,
                    anchor,
                    end_anchor,
                    text: text.to_string(),
                });
            } else {
                let Some(old_text) = edit_obj.get("old_text").and_then(|o| o.as_str()) else {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: missing 'old_text' (or 'edit_type' for an anchor edit)",
                        fi, ei
                    ));
                };
                let Some(new_text) = edit_obj.get("new_text").and_then(|n| n.as_str()) else {
                    return ToolOutput::error(format!(
                        "files[{}].edits[{}]: missing 'new_text'",
                        fi, ei
                    ));
                };
                edits.push(Edit::Legacy {
                    old_text: old_text.to_string(),
                    new_text: new_text.to_string(),
                });
            }
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
    //
    // For each file:
    //  - Compute per-line anchor tokens once (with disambiguators) and an
    //    index from token → line number.
    //  - For each edit, resolve a line range `[start, end]` (inclusive) on
    //    the file's *original* line vector. Anchor edits resolve via the
    //    anchor map; legacy edits resolve via `find` on the rejoined
    //    content and convert the byte span to a line span.
    //  - Reject overlapping ranges within a file.
    //  - Reject zero-match / multi-match legacy `old_text` (existing
    //    behaviour preserved with the same error strings).
    //  - Reject unknown / stale anchors with a clear "re-read" hint.
    let mut errors: Vec<String> = Vec::new();
    // Per-file: the resolved line ranges, stored alongside their original
    // edit index so we can drive apply order from this in Phase 5.
    let mut per_file_ranges: Vec<Vec<EditPlan>> = Vec::with_capacity(files.len());

    for (fi, f) in files.iter().enumerate() {
        let content = &originals[fi];
        // Split with `lines()` (drops trailing empty after final \n) so
        // anchor numbering matches the read tool exactly.
        let lines: Vec<&str> = content.lines().collect();
        let anchor_tokens = hash_lines_with_disambiguators(&lines);
        // token → line index. Tokens are unique by construction (the
        // disambiguator ensures it), so a HashMap is sufficient.
        let anchor_index: std::collections::HashMap<&str, usize> = anchor_tokens
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), i))
            .collect();

        let label_for = |ei: usize| -> String {
            if files.len() == 1 && f.edits.len() == 1 {
                String::new()
            } else if files.len() == 1 {
                format!("edits[{}]: ", ei)
            } else {
                format!("files[{}].edits[{}]: ", fi, ei)
            }
        };

        let mut plans: Vec<EditPlan> = Vec::with_capacity(f.edits.len());
        for (ei, edit) in f.edits.iter().enumerate() {
            match edit {
                Edit::Legacy { old_text, .. } => {
                    let count = content.matches(old_text.as_str()).count();
                    if count == 0 {
                        errors.push(format!(
                            "{}old_text not found in {}",
                            label_for(ei),
                            f.resolved.display()
                        ));
                        continue;
                    } else if count > 1 {
                        errors.push(format!(
                            "{}old_text found {} times in {}. Must be unique.",
                            label_for(ei),
                            count,
                            f.resolved.display()
                        ));
                        continue;
                    }
                    // Translate the byte span to a line range for overlap
                    // detection. Lines are 0-indexed here (internal use only);
                    // we never surface them to the model.
                    let byte_start = match content.find(old_text.as_str()) {
                        Some(b) => b,
                        None => continue, // unreachable given count == 1
                    };
                    let byte_end = byte_start + old_text.len();
                    let (line_start, line_end) =
                        byte_span_to_line_span(content, byte_start, byte_end);
                    plans.push(EditPlan {
                        edit_index: ei,
                        line_start,
                        line_end,
                    });
                }
                Edit::Anchor {
                    kind,
                    anchor,
                    end_anchor,
                    ..
                } => {
                    let Some(&start_idx) = anchor_index.get(anchor.as_str()) else {
                        errors.push(format!(
                            "{}anchor `{}` not found in {}; re-read the file to get current anchors",
                            label_for(ei),
                            anchor,
                            f.resolved.display()
                        ));
                        continue;
                    };
                    match kind {
                        AnchorKind::Replace => {
                            let Some(&end_idx) = anchor_index.get(end_anchor.as_str()) else {
                                errors.push(format!(
                                    "{}end_anchor `{}` not found in {}; re-read the file to get current anchors",
                                    label_for(ei),
                                    end_anchor,
                                    f.resolved.display()
                                ));
                                continue;
                            };
                            if end_idx < start_idx {
                                errors.push(format!(
                                    "{}end_anchor `{}` precedes anchor `{}` in {}",
                                    label_for(ei),
                                    end_anchor,
                                    anchor,
                                    f.resolved.display()
                                ));
                                continue;
                            }
                            plans.push(EditPlan {
                                edit_index: ei,
                                line_start: start_idx,
                                line_end: end_idx,
                            });
                        }
                        AnchorKind::InsertBefore | AnchorKind::InsertAfter => {
                            // Insertions don't consume lines for overlap
                            // purposes — model them as a zero-width range
                            // *between* lines, encoded as a single line
                            // index for simplicity.
                            plans.push(EditPlan {
                                edit_index: ei,
                                line_start: start_idx,
                                line_end: start_idx,
                            });
                        }
                    }
                }
            }
        }

        // Detect overlaps within this file. We only run the overlap check
        // when at least one anchor edit is present in this file's batch —
        // pure-legacy batches retain the prior behaviour of allowing two
        // edits on the same physical line as long as both `old_text`s are
        // unique (each `replacen` finds a single distinct byte span).
        let any_anchor = f.edits.iter().any(|e| matches!(e, Edit::Anchor { .. }));
        if errors.is_empty() && any_anchor {
            let mut sorted: Vec<&EditPlan> = plans.iter().collect();
            sorted.sort_by_key(|p| (p.line_start, p.line_end));
            for w in sorted.windows(2) {
                let a = w[0];
                let b = w[1];
                if a.line_end >= b.line_start {
                    errors.push(format!(
                        "{}overlaps with edits[{}] in {} (ranges {}-{} and {}-{})",
                        label_for(b.edit_index),
                        a.edit_index,
                        f.resolved.display(),
                        a.line_start + 1,
                        a.line_end + 1,
                        b.line_start + 1,
                        b.line_end + 1,
                    ));
                }
            }
        }

        per_file_ranges.push(plans);
    }
    if !errors.is_empty() {
        return ToolOutput::error(errors.join("\n"));
    }

    // ---- Phase 5: apply ----
    //
    // For each file: operate on a `Vec<String>` of lines, splice anchor
    // edits in reverse line-index order so earlier indices stay stable,
    // then rejoin and run any legacy `replacen` pass on the joined string.
    // Trailing-newline preservation: if the original ended with \n, the
    // updated file does too.
    let mut updated: Vec<String> = Vec::with_capacity(files.len());
    for (fi, f) in files.iter().enumerate() {
        let original = &originals[fi];
        let trailing_newline = original.ends_with('\n');
        let mut lines: Vec<String> = original.lines().map(|s| s.to_string()).collect();

        // Anchor edits, sorted by descending start so earlier-edit indices
        // don't shift under our feet. We resolve the same line ranges we
        // just validated (no re-validation needed).
        let mut anchor_plans: Vec<&EditPlan> = per_file_ranges[fi]
            .iter()
            .filter(|p| matches!(f.edits[p.edit_index], Edit::Anchor { .. }))
            .collect();
        anchor_plans.sort_by(|a, b| b.line_start.cmp(&a.line_start));

        for plan in anchor_plans {
            let edit = &f.edits[plan.edit_index];
            let Edit::Anchor { kind, text, .. } = edit else {
                continue;
            };
            // `text` may itself be multi-line. We split on '\n' so the
            // model doesn't have to think about line endings; an empty
            // `text` means "delete" for replace, or "insert nothing" for
            // the (unusual) insert case.
            let new_lines: Vec<String> = if text.is_empty() {
                Vec::new()
            } else {
                text.split('\n').map(|s| s.to_string()).collect()
            };
            match kind {
                AnchorKind::Replace => {
                    let _ = lines.splice(plan.line_start..=plan.line_end, new_lines);
                }
                AnchorKind::InsertBefore => {
                    let _ = lines.splice(plan.line_start..plan.line_start, new_lines);
                }
                AnchorKind::InsertAfter => {
                    let pos = plan.line_start + 1;
                    let _ = lines.splice(pos..pos, new_lines);
                }
            }
        }

        let mut content = lines.join("\n");
        if trailing_newline {
            content.push('\n');
        }

        // Legacy `old_text → new_text` edits run after anchor edits, on the
        // rejoined string. Validation already proved each `old_text`
        // existed exactly once and didn't overlap with any anchor edit; we
        // run them in input order via `replacen(_, _, 1)`.
        for edit in &f.edits {
            if let Edit::Legacy { old_text, new_text } = edit {
                content = content.replacen(old_text.as_str(), new_text.as_str(), 1);
            }
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
    let mut total_added = 0usize;
    let mut total_removed = 0usize;
    for (fi, f) in files.iter().enumerate() {
        for (ei, edit) in f.edits.iter().enumerate() {
            let plan = per_file_ranges[fi]
                .iter()
                .find(|p| p.edit_index == ei)
                .expect("every edit has a plan after validation");
            let (added, removed) = edit_line_deltas(edit, plan);
            total_added += added;
            total_removed += removed;
        }
    }

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

    #[test]
    fn mixed_shapes_rejected_files_plus_path_and_edits() {
        // Schema can't structurally forbid this anymore (Anthropic rejects
        // top-level oneOf), so execute validates it at runtime.
        let (_dir, path) = setup_file("hello world");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "edits": [{"old_text": "hello", "new_text": "goodbye"}],
                "files": [{
                    "path": path.to_str().expect("path to str"),
                    "edits": [{"old_text": "hello", "new_text": "goodbye"}]
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(
            msg.contains("EITHER") || msg.contains("both"),
            "error should mention mutual exclusion, got: {msg}"
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "hello world"
        );
    }

    #[test]
    fn mixed_shapes_rejected_files_plus_path_only() {
        // Even just a stray top-level `path` next to `files` is rejected.
        let (_dir, path) = setup_file("hello world");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path to str"),
                "files": [{
                    "path": path.to_str().expect("path to str"),
                    "edits": [{"old_text": "hello", "new_text": "goodbye"}]
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(
            msg.contains("EITHER") || msg.contains("both"),
            "error should mention mutual exclusion, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            "hello world"
        );
    }

    #[test]
    fn empty_object_rejected() {
        // Neither shape provided — must error with a clear message that
        // names both shapes, since the schema can no longer enforce it.
        let result = run_tool(serde_json::json!({}), "/tmp");
        assert!(result.is_error);
        let msg = result.content[0].text();
        assert!(
            msg.contains("files") && msg.contains("path"),
            "error should reference both shapes, got: {msg}"
        );
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

    // --- anchor-shape edits ----------------------------------------------

    fn anchor(line: &str) -> String {
        super::super::line_hash::fnv1a_8hex(line)
    }

    #[test]
    fn anchor_replace_single_line() {
        let (_dir, path) = setup_file("alpha\nbeta\ngamma\n");
        let h_beta = anchor("beta");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_beta,
                    "text": "BETA"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[test]
    fn anchor_replace_range_inclusive() {
        let (_dir, path) = setup_file("a\nb\nc\nd\ne\n");
        let h_b = anchor("b");
        let h_d = anchor("d");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_b,
                    "end_anchor": h_d,
                    "text": "X\nY"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        // Inclusive: b, c, d are removed; X, Y inserted.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "a\nX\nY\ne\n"
        );
    }

    #[test]
    fn anchor_replace_with_empty_text_deletes_range() {
        let (_dir, path) = setup_file("a\nb\nc\n");
        let h_b = anchor("b");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_b,
                    "text": ""
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "a\nc\n");
    }

    #[test]
    fn anchor_insert_before() {
        let (_dir, path) = setup_file("a\nb\nc\n");
        let h_b = anchor("b");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "insert_before",
                    "anchor": h_b,
                    "text": "X\nY"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "a\nX\nY\nb\nc\n"
        );
    }

    #[test]
    fn anchor_insert_after() {
        let (_dir, path) = setup_file("a\nb\nc\n");
        let h_b = anchor("b");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "insert_after",
                    "anchor": h_b,
                    "text": "X"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "a\nb\nX\nc\n"
        );
    }

    #[test]
    fn anchor_full_form_with_delimiter_accepted() {
        // The model is allowed to copy the entire `<hash>§<line>` token
        // straight from the read tool's output — we strip everything from
        // the first § onward.
        let (_dir, path) = setup_file("    def foo():\n    pass\n");
        let h = anchor("    def foo():");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": format!("{}§    def foo():", h),
                    "text": "    def bar():"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "    def bar():\n    pass\n"
        );
    }

    #[test]
    fn anchor_with_disambiguator() {
        // Two identical lines — the second carries `<hash>.2`.
        let (_dir, path) = setup_file("foo\nfoo\nbar\n");
        let h_foo = anchor("foo");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": format!("{}.2", h_foo),
                    "text": "FOO2"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "foo\nFOO2\nbar\n"
        );
    }

    #[test]
    fn anchor_unknown_rejected() {
        let (_dir, path) = setup_file("alpha\nbeta\n");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": "deadbeef",
                    "text": "X"
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("anchor `deadbeef` not found"), "got: {msg}");
        assert!(msg.contains("re-read"), "got: {msg}");
        // File unchanged.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "alpha\nbeta\n"
        );
    }

    #[test]
    fn anchor_stale_hash_rejected() {
        // Simulate "file changed since read": pass an anchor whose hash
        // matches a *different* line in the current file. Validation must
        // either find the token (then mismatch → error) or not find it
        // (also error). Either way, the file must not be modified.
        let (_dir, path) = setup_file("alpha\nbeta\n");
        // Pretend the model read the file when it contained "old line" and
        // is now passing that anchor. After the model's read the file was
        // changed (here: it never contained "old line").
        let stale = anchor("old line");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": stale,
                    "text": "X"
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "alpha\nbeta\n"
        );
    }

    #[test]
    fn anchor_replace_missing_end_anchor_defaults_to_single_line() {
        let (_dir, path) = setup_file("a\nb\nc\n");
        let h_b = anchor("b");
        // No `end_anchor` → single-line replace at `anchor`.
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_b,
                    "text": "B"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "a\nB\nc\n");
    }

    #[test]
    fn anchor_replace_end_before_start_rejected() {
        let (_dir, path) = setup_file("a\nb\nc\n");
        let h_a = anchor("a");
        let h_c = anchor("c");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_c,
                    "end_anchor": h_a,
                    "text": "X"
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("precedes"), "got: {msg}");
        // File unchanged.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "a\nb\nc\n");
    }

    #[test]
    fn unknown_edit_type_rejected() {
        let (_dir, path) = setup_file("a\n");
        let h_a = anchor("a");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "clobber",
                    "anchor": h_a,
                    "text": "X"
                }]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("unknown edit_type"), "got: {msg}");
    }

    #[test]
    fn mixed_anchor_and_legacy_edits_in_one_call() {
        let (_dir, path) = setup_file("alpha\nbeta\ngamma\ndelta\n");
        let h_beta = anchor("beta");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [
                    { "edit_type": "replace", "anchor": h_beta, "text": "BETA" },
                    { "old_text": "delta", "new_text": "DELTA" }
                ]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "alpha\nBETA\ngamma\nDELTA\n"
        );
    }

    #[test]
    fn overlapping_anchor_edits_rejected() {
        let (_dir, path) = setup_file("a\nb\nc\nd\n");
        let h_a = anchor("a");
        let h_c = anchor("c");
        let h_b = anchor("b");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [
                    { "edit_type": "replace", "anchor": h_a, "end_anchor": h_c, "text": "X" },
                    { "edit_type": "replace", "anchor": h_b, "text": "Y" }
                ]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("overlap"), "got: {msg}");
        // File unchanged.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "a\nb\nc\nd\n"
        );
    }

    #[test]
    fn anchor_edit_preserves_no_trailing_newline() {
        // If the original file lacked a final \n, the rewrite must not add one.
        let (_dir, path) = setup_file("alpha\nbeta");
        let h_beta = anchor("beta");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{ "edit_type": "replace", "anchor": h_beta, "text": "BETA" }]
            }),
            "/tmp",
        );
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "alpha\nBETA");
    }

    #[test]
    fn anchor_edit_summary_tracks_added_removed_lines() {
        // A 3-line replace with 2 lines of replacement → +2 -3.
        let (_dir, path) = setup_file("a\nb\nc\nd\ne\n");
        let h_b = anchor("b");
        let h_d = anchor("d");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{
                    "edit_type": "replace",
                    "anchor": h_b,
                    "end_anchor": h_d,
                    "text": "X\nY"
                }]
            }),
            "/tmp",
        );
        assert!(!result.is_error);
        let summary = result.summary.expect("summary");
        assert!(summary.contains("+2 -3"), "got: {summary}");
    }

    #[test]
    fn anchor_in_multifile_form() {
        // Anchor edits work in the multi-file form too.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "alpha\nA\n").expect("write");
        std::fs::write(&b, "beta\nB\n").expect("write");
        let h_a = anchor("alpha");
        let h_b = anchor("beta");
        let result = run_tool(
            serde_json::json!({
                "files": [
                    {"path": a.to_str().expect("path"),
                     "edits": [{"edit_type": "replace", "anchor": h_a, "text": "ALPHA"}]},
                    {"path": b.to_str().expect("path"),
                     "edits": [{"edit_type": "replace", "anchor": h_b, "text": "BETA"}]}
                ]
            }),
            "/tmp",
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        assert_eq!(std::fs::read_to_string(&a).expect("read a"), "ALPHA\nA\n");
        assert_eq!(std::fs::read_to_string(&b).expect("read b"), "BETA\nB\n");
    }

    #[test]
    fn anchor_missing_text_rejected() {
        let (_dir, path) = setup_file("a\n");
        let h_a = anchor("a");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{"edit_type": "replace", "anchor": h_a}]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("missing 'text'"));
    }

    #[test]
    fn anchor_missing_anchor_rejected() {
        let (_dir, path) = setup_file("a\n");
        let result = run_tool(
            serde_json::json!({
                "path": path.to_str().expect("path"),
                "edits": [{"edit_type": "replace", "text": "X"}]
            }),
            "/tmp",
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("missing 'anchor'"));
    }
}
