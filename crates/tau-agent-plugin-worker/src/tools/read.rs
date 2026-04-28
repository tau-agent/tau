//! Read tool — read the contents of one or more files.
//!
//! Input shape is `paths: [string, ...]` with optional `offset` / `limit`
//! applied per file. A pre-validation [`prepare_arguments`] hook silently
//! folds the legacy single-path `{path, offset?, limit?}` shape into
//! `{paths: [path], …}`, so resumed sessions whose history predates the
//! multi-path schema keep working without polluting the public schema shown
//! to the model.
//!
//! Each line in the body is prefixed with `<hash>§<line>` (or
//! `<hash>.<n>§<line>` for the 2nd+ identical line in a single file). Hash
//! is FNV-1a 32-bit rendered as 8 lowercase hex chars. The model can use
//! these anchors with the `edit` tool's anchor shape (`{edit_type, anchor,
//! end_anchor?, text}`). See `super::line_hash` for the hashing module.
//!
//! Output:
//! - **Single path** (after folding): bytes-for-bytes identical to the old
//!   single-file format — body and summary unchanged. This is a regression
//!   guard for transcripts on resumed sessions.
//! - **Multiple paths**: per-file sections, each headed by `===== <path> =====`,
//!   separated by a blank line. A failed file renders an inline `error: …`
//!   in its section but does not block other files (partial success). The
//!   call is reported as an error only when *every* file failed.
//!
//! Caps:
//! - At most [`MAX_PATHS`] paths per call.
//! - At most [`MAX_TOTAL_BYTES`] bytes of file content (post offset/limit
//!   slicing) returned across the whole call. When the cap is reached
//!   mid-iteration, remaining files are replaced with a truncation marker.
//!
//! Duplicate paths are read independently and each occurrence counts against
//! the byte cap on its own — the simplest behaviour, matches what the model
//! probably expected.

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

/// Maximum number of paths accepted in a single `read` call. Prevents the
/// model from accidentally enumerating a whole tree.
pub const MAX_PATHS: usize = 20;

/// Maximum total bytes of file content returned across all paths in a single
/// call (post offset/limit slicing). Once reached, remaining files are
/// dropped with a truncation marker rather than ballooning the response.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "read".into(),
            description:
                "Read the contents of one or more files. Supports offset/limit for large files (applied per file). Each line in the body is prefixed with `<hash>§` — a stable per-line anchor (FNV-1a 8 hex; `.n` suffix for duplicate lines) you can use with the `edit` tool's anchor shape. Prefer `get_file_skeleton` to outline a file's structure cheaply, or `get_function` to pull a specific function body, when you don't need the whole file."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_PATHS,
                        "description": "Paths to the files to read (1–20)."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed). Applied to each file independently."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read per file."
                    }
                },
                "required": ["paths"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: Some(Box::new(prepare_arguments)),
    }
}

/// Fold a legacy `{path: "..."}` tool-call argument into the multi-path
/// `{paths: ["..."]}` shape. Used as the `prepare_arguments` hook so resumed
/// sessions that recorded the old shape in their history validate cleanly.
///
/// If the input isn't an object, already carries a `paths` field, or has a
/// non-string `path`, it is returned unchanged.
fn prepare_arguments(mut args: serde_json::Value) -> serde_json::Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };

    if obj.contains_key("paths") {
        return args;
    }

    let Some(path_val) = obj.get("path") else {
        return args;
    };
    if !path_val.is_string() {
        return args;
    }

    let Some(path_val) = obj.remove("path") else {
        return args;
    };
    obj.insert(
        "paths".to_string(),
        serde_json::Value::Array(vec![path_val]),
    );

    args
}

/// Per-file outcome — either a rendered body + summary fragment, or an
/// inline error string to render in the file's section.
struct FileRead {
    /// Path string as the caller requested it (echoed back verbatim).
    path_str: String,
    /// On success: rendered body (joined lines + optional continuation hint).
    /// On failure: `Err(message)` rendered as an inline `error: …` line.
    body: Result<String, String>,
    /// Total lines in the file (used for the multi-file summary). Zero on
    /// error.
    total_lines: usize,
    /// Bytes the body contributes to the global byte cap. Zero on error.
    bytes: usize,
    /// `Some((start1, end))` when offset/limit selected a sub-range; used
    /// for the single-file summary path. `None` for full reads or errors.
    range: Option<(usize, usize)>,
}

fn read_one(
    cwd: &str,
    path_str: &str,
    offset: usize,
    limit: Option<usize>,
    remaining_bytes: usize,
) -> FileRead {
    let path = super::resolve_path(cwd, path_str);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return FileRead {
                path_str: path_str.to_string(),
                body: Err(format!("failed to read {}: {}", path.display(), e)),
                total_lines: 0,
                bytes: 0,
                range: None,
            };
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = (offset.max(1) - 1).min(total);
    let end = match limit {
        Some(l) => (start + l).min(total),
        None => total,
    };

    // Hash anchors are computed over the *whole file* so the disambiguator
    // counts (`.2`, `.3`, ...) reflect the file as a whole, not just the
    // visible slice. The model can then refer to any line in the file even
    // if it's outside the offset/limit window of this particular read.
    let all_anchors = super::line_hash::hash_lines_with_disambiguators(&lines);
    let selected = &lines[start..end];
    let selected_anchors = &all_anchors[start..end];
    let mut body = super::line_hash::format_hashed(selected, selected_anchors);

    if end < total {
        body.push_str(&format!(
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            total - end,
            end + 1,
        ));
    }

    // Enforce the per-call byte cap by truncating this file's body if it
    // would push past the remaining budget. We slice on a char boundary to
    // keep `String` valid; the marker tells the model what happened.
    let bytes = body.len();
    let (body, bytes) = if bytes > remaining_bytes {
        let mut cut = remaining_bytes.min(body.len());
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut truncated: String = body[..cut].to_string();
        truncated.push_str("\n\n[truncated: per-call byte cap reached]");
        let trunc_bytes = truncated.len();
        (truncated, trunc_bytes)
    } else {
        (body, bytes)
    };

    let range = if start == 0 && end == total {
        None
    } else {
        Some((start + 1, end))
    };

    FileRead {
        path_str: path_str.to_string(),
        body: Ok(body),
        total_lines: total,
        bytes,
        range,
    }
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    _cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    let Some(paths_val) = args.get("paths") else {
        return ToolOutput::error("missing 'paths' argument (expected an array of strings)");
    };
    let Some(paths_arr) = paths_val.as_array() else {
        return ToolOutput::error("'paths' must be an array of strings");
    };
    if paths_arr.is_empty() {
        return ToolOutput::error("'paths' array is empty — provide at least one path");
    }
    if paths_arr.len() > MAX_PATHS {
        return ToolOutput::error(format!(
            "too many paths: {} (max {})",
            paths_arr.len(),
            MAX_PATHS
        ));
    }

    let mut paths: Vec<String> = Vec::with_capacity(paths_arr.len());
    for (i, v) in paths_arr.iter().enumerate() {
        let Some(s) = v.as_str() else {
            return ToolOutput::error(format!("paths[{}] is not a string", i));
        };
        paths.push(s.to_string());
    }

    let offset = args
        .get("offset")
        .and_then(|o| o.as_u64())
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(|l| l.as_u64())
        .map(|l| l as usize);

    let n_paths = paths.len();
    let mut results: Vec<FileRead> = Vec::with_capacity(n_paths);
    let mut used_bytes: usize = 0;
    let mut cap_reached = false;
    let mut skipped_after_cap: usize = 0;

    for path_str in &paths {
        if cap_reached {
            skipped_after_cap += 1;
            continue;
        }
        let remaining = MAX_TOTAL_BYTES.saturating_sub(used_bytes);
        let fr = read_one(cwd, path_str, offset, limit, remaining);
        used_bytes = used_bytes.saturating_add(fr.bytes);
        if used_bytes >= MAX_TOTAL_BYTES {
            cap_reached = true;
        }
        results.push(fr);
    }

    // Render output. Single-path calls get the legacy format byte-for-byte.
    if n_paths == 1 {
        // Safe: we just pushed exactly one entry above.
        let fr = results.into_iter().next().expect("one result for one path");
        match fr.body {
            Ok(body) => {
                let summary = match fr.range {
                    None => format!("read: {} ({} lines)", fr.path_str, fr.total_lines),
                    Some((s, e)) => format!(
                        "read: {} (lines {}-{}, {} total)",
                        fr.path_str, s, e, fr.total_lines
                    ),
                };
                ToolOutput::text(body).with_summary(summary)
            }
            Err(msg) => ToolOutput::error(msg),
        }
    } else {
        let mut out = String::new();
        let mut total_lines = 0usize;
        let mut errors = 0usize;
        let mut successes = 0usize;
        for (i, fr) in results.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&format!("===== {} =====\n", fr.path_str));
            match &fr.body {
                Ok(body) => {
                    out.push_str(body);
                    total_lines += fr.total_lines;
                    successes += 1;
                }
                Err(msg) => {
                    out.push_str("error: ");
                    out.push_str(msg);
                    errors += 1;
                }
            }
        }
        if cap_reached && skipped_after_cap > 0 {
            out.push_str(&format!(
                "\n\n[truncated: byte cap reached, {} file(s) not read]",
                skipped_after_cap
            ));
        }

        let summary = if errors == 0 {
            format!("read: {} files ({} total lines)", n_paths, total_lines)
        } else {
            format!(
                "read: {} files ({} total lines, {} error{})",
                n_paths,
                total_lines,
                errors,
                if errors == 1 { "" } else { "s" }
            )
        };

        let mut output = ToolOutput::text(out).with_summary(summary);
        // Partial success is not an error; only flag the call as errored
        // when *every* path failed.
        output.is_error = successes == 0;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).expect("write test file");
        p
    }

    #[test]
    fn multi_path_happy_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.txt", "alpha-1\nalpha-2\n");
        let b = write_file(dir.path(), "b.txt", "bravo-1\nbravo-2\nbravo-3\n");
        let result = execute(
            serde_json::json!({
                "paths": [
                    a.to_str().expect("a path"),
                    b.to_str().expect("b path"),
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        let text = result.content[0].text().to_string();
        assert!(text.contains(&format!("===== {} =====", a.to_str().expect("a path"))));
        assert!(text.contains(&format!("===== {} =====", b.to_str().expect("b path"))));
        assert!(text.contains("alpha-1"));
        assert!(text.contains("bravo-3"));
        let summary = result.summary.expect("summary");
        assert!(summary.starts_with("read: 2 files"), "got: {summary}");
        assert!(summary.contains("5 total lines"), "got: {summary}");
        assert!(!summary.contains("error"), "got: {summary}");
    }

    #[test]
    fn partial_success_one_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.txt", "alpha\n");
        let missing = dir.path().join("missing.txt");
        let result = execute(
            serde_json::json!({
                "paths": [
                    a.to_str().expect("a path"),
                    missing.to_str().expect("missing path"),
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "partial success should not be error");
        let text = result.content[0].text().to_string();
        assert!(text.contains("alpha"));
        assert!(text.contains("error: failed to read"));
        let summary = result.summary.expect("summary");
        assert!(summary.contains("1 error"), "got: {summary}");
    }

    #[test]
    fn legacy_single_path_shape_matches_old_format() {
        // Single-path (legacy fold) keeps the legacy summary format and
        // header-less body. Each line now carries an `<hash>§` prefix —
        // that is the only intentional change.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write_file(dir.path(), "f.txt", "line1\nline2\nline3\n");
        let p_str = p.to_str().expect("path str");

        // Drive through prepare_arguments + execute as the agent loop does.
        let prepared = prepare_arguments(serde_json::json!({"path": p_str}));
        let result = execute(prepared, "/tmp", &tau_agent_plugin::CancelToken::new());
        assert!(!result.is_error, "got: {:?}", result.content);
        let text = result.content[0].text().to_string();
        let h1 = super::super::line_hash::fnv1a_8hex("line1");
        let h2 = super::super::line_hash::fnv1a_8hex("line2");
        let h3 = super::super::line_hash::fnv1a_8hex("line3");
        assert_eq!(text, format!("{h1}§line1\n{h2}§line2\n{h3}§line3"));
        assert_eq!(
            result.summary.expect("summary"),
            format!("read: {} (3 lines)", p_str)
        );

        // Also drive through execute_tool against default_tools() to exercise
        // the wired-up prepare hook.
        use super::super::{ToolDef, default_tools, execute_tool};
        use tau_agent_plugin::ToolCall;
        let tools: Vec<ToolDef> = default_tools();
        let tc = ToolCall {
            id: "tc1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": p_str}),
        };
        let r = execute_tool(&tools, &tc, "/tmp", &tau_agent_plugin::CancelToken::new());
        assert!(!r.is_error);
        let body = r.content[0].text().to_string();
        assert_eq!(body, format!("{h1}§line1\n{h2}§line2\n{h3}§line3"));
        assert_eq!(
            r.summary.expect("summary"),
            format!("read: {} (3 lines)", p_str)
        );
    }

    #[test]
    fn offset_and_limit_applied_per_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let short = write_file(dir.path(), "short.txt", "s1\ns2\ns3\n"); // 3 lines
        let long = write_file(dir.path(), "long.txt", "l1\nl2\nl3\nl4\nl5\nl6\n"); // 6 lines
        let result = execute(
            serde_json::json!({
                "paths": [
                    short.to_str().expect("short path"),
                    long.to_str().expect("long path"),
                ],
                "offset": 2,
                "limit": 2,
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error, "got: {:?}", result.content);
        let text = result.content[0].text().to_string();
        // short.txt with offset=2,limit=2 → s2, s3 (no continuation, end of file)
        let hs2 = super::super::line_hash::fnv1a_8hex("s2");
        let hs3 = super::super::line_hash::fnv1a_8hex("s3");
        assert!(
            text.contains(&format!("{hs2}§s2\n{hs3}§s3")),
            "missing short slice in: {text}"
        );
        assert!(
            !text.contains("[1 more lines"),
            "short.txt should not have continuation hint: {text}"
        );
        // long.txt with offset=2,limit=2 → l2, l3 + continuation hint (4 more)
        let hl2 = super::super::line_hash::fnv1a_8hex("l2");
        let hl3 = super::super::line_hash::fnv1a_8hex("l3");
        assert!(
            text.contains(&format!("{hl2}§l2\n{hl3}§l3")),
            "missing long slice in: {text}"
        );
        assert!(
            text.contains("[3 more lines in file. Use offset=4 to continue.]"),
            "missing continuation hint in: {text}"
        );
    }

    #[test]
    fn path_count_cap_rejects() {
        // 21 paths → top-level error, no files read.
        let many: Vec<String> = (0..(MAX_PATHS + 1))
            .map(|i| format!("/nonexistent/{i}.txt"))
            .collect();
        let result = execute(
            serde_json::json!({"paths": many}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        let msg = result.content[0].text().to_string();
        assert!(msg.contains("too many paths"), "got: {msg}");
    }

    #[test]
    fn total_byte_cap_truncates_remaining() {
        // Three files where each is just under half the cap. The first two
        // fit; the third gets dropped with a truncation marker.
        let dir = tempfile::tempdir().expect("tempdir");
        let big_line = "x".repeat(1024); // 1 KiB per line
        let body = (0..120)
            .map(|_| big_line.as_str())
            .collect::<Vec<_>>()
            .join("\n"); // ~120 KiB per file
        let a = write_file(dir.path(), "a.txt", &body);
        let b = write_file(dir.path(), "b.txt", &body);
        let c = write_file(dir.path(), "c.txt", &body);
        let result = execute(
            serde_json::json!({
                "paths": [
                    a.to_str().expect("a"),
                    b.to_str().expect("b"),
                    c.to_str().expect("c"),
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        let text = result.content[0].text().to_string();
        // c.txt was either truncated mid-body or skipped entirely with a
        // marker. Either way, the global cap marker should appear OR the
        // third file's body should carry the per-file truncation marker.
        assert!(
            text.contains("[truncated:"),
            "expected a truncation marker in: {}…",
            &text[..text.len().min(300)]
        );
    }

    #[test]
    fn all_paths_fail_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m1 = dir.path().join("missing1.txt");
        let m2 = dir.path().join("missing2.txt");
        let result = execute(
            serde_json::json!({
                "paths": [
                    m1.to_str().expect("m1"),
                    m2.to_str().expect("m2"),
                ]
            }),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error, "all-failed must be flagged as error");
    }

    #[test]
    fn empty_paths_array_rejected() {
        let result = execute(
            serde_json::json!({"paths": []}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("empty"));
    }

    #[test]
    fn non_string_path_entry_rejected() {
        let result = execute(
            serde_json::json!({"paths": ["ok.txt", 42]}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("paths[1]"));
    }

    #[test]
    fn missing_paths_argument_rejected() {
        let result = execute(
            serde_json::json!({}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(result.is_error);
        assert!(result.content[0].text().contains("paths"));
    }

    // --- prepare_arguments (legacy input fold) ---------------------------

    #[test]
    fn prepare_arguments_folds_legacy_path() {
        let input = serde_json::json!({"path": "x.txt"});
        let prepared = prepare_arguments(input);
        assert_eq!(prepared, serde_json::json!({"paths": ["x.txt"]}));
    }

    #[test]
    fn prepare_arguments_folds_legacy_path_with_offset_limit() {
        let input = serde_json::json!({"path": "x.txt", "offset": 5, "limit": 10});
        let prepared = prepare_arguments(input);
        assert_eq!(
            prepared,
            serde_json::json!({"paths": ["x.txt"], "offset": 5, "limit": 10})
        );
    }

    #[test]
    fn prepare_arguments_passthrough_when_paths_present() {
        let input = serde_json::json!({"paths": ["a.txt", "b.txt"]});
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_passthrough_when_paths_present_and_legacy_path_present() {
        // If both shapes coexist (defensive), prefer the new shape and
        // leave the legacy `path` field alone — downstream just uses paths.
        let input = serde_json::json!({"paths": ["a.txt"], "path": "ignored.txt"});
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_passthrough_non_object() {
        let input = serde_json::json!("not an object");
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    #[test]
    fn prepare_arguments_passthrough_non_string_path() {
        let input = serde_json::json!({"path": 42});
        let prepared = prepare_arguments(input.clone());
        assert_eq!(prepared, input);
    }

    // --- hash anchor output -----------------------------------------------

    #[test]
    fn hashed_output_uses_section_delimiter_per_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write_file(dir.path(), "f.txt", "alpha\nbeta\n");
        let p_str = p.to_str().expect("path");
        let result = execute(
            serde_json::json!({"paths": [p_str]}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        let text = result.content[0].text().to_string();
        let h_alpha = super::super::line_hash::fnv1a_8hex("alpha");
        let h_beta = super::super::line_hash::fnv1a_8hex("beta");
        assert_eq!(text, format!("{h_alpha}§alpha\n{h_beta}§beta"));
    }

    #[test]
    fn duplicate_lines_get_disambiguators() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Three identical lines → hash, hash.2, hash.3.
        let p = write_file(dir.path(), "f.txt", "foo\nbar\nfoo\nfoo\n");
        let p_str = p.to_str().expect("path");
        let result = execute(
            serde_json::json!({"paths": [p_str]}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        let text = result.content[0].text().to_string();
        let h_foo = super::super::line_hash::fnv1a_8hex("foo");
        let h_bar = super::super::line_hash::fnv1a_8hex("bar");
        let expected = format!("{h_foo}§foo\n{h_bar}§bar\n{h_foo}.2§foo\n{h_foo}.3§foo");
        assert_eq!(text, expected);
    }

    #[test]
    fn empty_file_produces_empty_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write_file(dir.path(), "empty.txt", "");
        let p_str = p.to_str().expect("path");
        let result = execute(
            serde_json::json!({"paths": [p_str]}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        let text = result.content[0].text().to_string();
        assert_eq!(text, "");
        let summary = result.summary.expect("summary");
        assert_eq!(summary, format!("read: {} (0 lines)", p_str));
    }

    #[test]
    fn anchors_consistent_across_offset_window() {
        // The disambiguator counter must reflect the *whole file*, not just
        // the visible slice. If the model sees the second occurrence of a
        // duplicate line via offset=2, it must still see `<hash>.2`.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write_file(dir.path(), "f.txt", "foo\nfoo\nfoo\n");
        let p_str = p.to_str().expect("path");
        let result = execute(
            serde_json::json!({"paths": [p_str], "offset": 2, "limit": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!result.is_error);
        let text = result.content[0].text().to_string();
        let h_foo = super::super::line_hash::fnv1a_8hex("foo");
        assert_eq!(text, format!("{h_foo}.2§foo\n{h_foo}.3§foo"));
    }
}
