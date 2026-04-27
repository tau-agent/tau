//! `get_file_skeleton` tool — emit the structural outline (classes,
//! functions, methods, type aliases, …) of one or more source files using
//! tree-sitter.
//!
//! Inspired by Dirac (`src/services/tree-sitter/`). The queries are direct
//! ports of Dirac's `.scm` per-language captures, restricted to the
//! `@name.definition.*` clauses we care about for v1 — we drop the
//! `@name.reference` clauses since we don't do call graphs.
//!
//! ## Output shape
//!
//! For each path the tool emits a `===== <path> =====` header followed by
//! one *header line* per top-level or nested definition, in source order
//! and deduped by start row. Implementation bodies are never literally
//! stripped; we just take the line containing the definition's name node,
//! which is its signature line. That keeps the line verbatim — `pub`,
//! `async`, decorators, generics, attributes are all preserved cheaply and
//! the model gets enough to decide whether to read the file in full.
//!
//! Per-file errors (unsupported extension, IO error, parse failure) are
//! rendered inline as `error: …` blocks; the call as a whole is only
//! flagged `is_error` when *every* requested path failed. This mirrors how
//! `read` handles partial success.
//!
//! ## Supported languages (v1)
//!
//! - Rust (`.rs`)
//! - Python (`.py`, `.pyi`)
//! - JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`)
//! - TypeScript (`.ts`)
//! - TSX (`.tsx`)
//!
//! Other extensions return a per-file `no skeleton support for .<ext>; use
//! \`read\` instead` error per spec.

use super::tree_sitter_support::{self, Lang};
use super::{ToolDef, ToolOutput};
use std::collections::BTreeMap;
use tau_agent_plugin::Tool;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

/// Maximum number of paths accepted in a single call. Mirrors `read`'s cap
/// so the model has consistent budgeting between the two survey tools.
pub const MAX_PATHS: usize = 20;

/// Maximum total bytes of output across all files. Skeletons are tiny in
/// practice (one line per def), but we cap defensively in case someone runs
/// this against a generated file with thousands of definitions.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "get_file_skeleton".into(),
            description:
                "Return the structural outline (classes / functions / methods, including nested) of one or more source files, with implementation bodies omitted. Use this to skim several files cheaply before reading them in full. Supports Rust, Python, JavaScript, TypeScript, and TSX in v1; other extensions return a per-file error suggesting `read`."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_PATHS,
                        "description": "Paths to source files to outline (1–20)."
                    }
                },
                "required": ["paths"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: None,
    }
}

// ----- extraction ------------------------------------------------------

/// Parse `source` with `lang`'s grammar and return the *header lines* of
/// every definition captured by the query, sorted by source position and
/// deduped by start row.
///
/// We render one logical line per definition: the slice of source from
/// the start of the definition node up to (but not including) its body
/// (`{` for Rust/JS/TS, the indented block after `:` for Python). When
/// that signature spans multiple physical lines — the common case for
/// long Rust function signatures with one parameter per line — we join
/// them onto a single virtual line, preserving the indentation of the
/// first line and collapsing internal whitespace runs to a single space.
/// Single-line signatures are emitted verbatim, byte-for-byte.
fn extract(source: &str, lang: Lang) -> Result<Vec<String>, String> {
    let tree = tree_sitter_support::parse(lang, source)?;
    let query = tree_sitter_support::query_for(lang);

    let mut cursor = QueryCursor::new();
    // Map: row of the name node → rendered header line. BTreeMap keeps
    // source order and dedupes when multiple query patterns fire on the
    // same definition (e.g. trait method captured both as method and as
    // function-signature). First write wins; later identical rows are
    // ignored.
    let mut headers: BTreeMap<usize, String> = BTreeMap::new();
    let lines: Vec<&str> = source.lines().collect();
    let capture_names = query.capture_names();

    let mut it = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = it.next() {
        // Each match emits at least one `name.definition.*` capture and
        // one `definition.*` capture (modulo a couple of Python rules
        // that use `@definition.symbol`). Pair them up: the name row is
        // the dedupe key; the definition node bounds the signature.
        let mut name_node: Option<Node> = None;
        let mut def_node: Option<Node> = None;
        for cap in m.captures {
            let name = capture_names
                .get(cap.index as usize)
                .copied()
                .unwrap_or_default();
            if name.starts_with("name.definition") {
                // Multiple name captures in one match shouldn't happen
                // in practice, but if they do prefer the first.
                if name_node.is_none() {
                    name_node = Some(cap.node);
                }
            } else if name.starts_with("definition") {
                // Prefer the *smallest* enclosing definition — for the
                // Python decorated_definition rule there is only one
                // outer `@definition.symbol`, but for the Rust impl
                // `name.definition.class` rule we want the impl_item
                // itself, which is the only `@definition.*` capture.
                def_node = Some(cap.node);
            }
        }
        let Some(name_node) = name_node else {
            continue;
        };
        let row = name_node.start_position().row;
        if headers.contains_key(&row) {
            continue;
        }
        // Fall back to the name node when we somehow didn't get a
        // `definition.*` capture — degrades gracefully to the old
        // single-line behaviour.
        let def_node = def_node.unwrap_or(name_node);
        if let Some(line) = render_signature(source, &lines, lang, name_node, def_node) {
            headers.insert(row, line);
        }
    }

    Ok(headers.into_values().collect())
}

/// Render the header line for `def_node`. The signature is the slice
/// from the node's start byte through (and including) the body opener
/// — `{` for braced languages, `:` for Python — or the entire node when
/// there is no body (`pub type Alias = i32;`, trait method signatures).
///
/// If the signature fits on a single source line we emit that source
/// line verbatim, preserving the trailing `}`/`;` a one-liner would
/// have (e.g. `pub fn one() {}`). If it spans multiple lines we join
/// them onto one virtual line: leading indentation of the first line is
/// preserved, internal whitespace runs (including the linebreaks
/// between argument-list lines) are collapsed to single spaces.
fn render_signature(
    source: &str,
    lines: &[&str],
    lang: Lang,
    name_node: Node,
    def_node: Node,
) -> Option<String> {
    // Back up to the start of the line so the leading indentation of
    // the first signature line is included — tree-sitter's
    // `start_byte()` skips leading whitespace, but the indent carries
    // nesting information we want in the rendered header.
    let start_byte = line_start_byte(source, def_node.start_byte());
    let start_row = def_node.start_position().row;
    let end_byte = signature_end_byte(source, lang, def_node)?;
    if end_byte <= start_byte || end_byte > source.len() {
        return Some(lines.get(name_node.start_position().row)?.to_string());
    }
    let raw = source.get(start_byte..end_byte)?;
    if !raw.contains('\n') {
        // Single-line signature: emit the *full* source line so any
        // body that fits on the same line (e.g. `pub fn one() {}`,
        // `pub fn first(&self) -> i32 { 1 }`) survives the strip.
        return Some(lines.get(start_row)?.to_string());
    }
    Some(join_multiline(raw))
}

/// Walk back from `byte` to the byte just after the previous newline
/// (or to 0 if `byte` is on the first line). The result is always a
/// valid char boundary because `\n` is single-byte ASCII.
fn line_start_byte(source: &str, byte: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = byte.min(bytes.len());
    while i > 0 && bytes[i - 1] != b'\n' {
        i -= 1;
    }
    i
}

/// Collapse a multi-line signature onto one virtual line.
///
/// Rules:
/// - The leading indentation of the *first* line is preserved so the
///   rendered header still conveys nesting depth.
/// - Internal whitespace runs that span a newline are normalised: they
///   collapse to nothing when the run is adjacent to a "joiner"
///   character — i.e. immediately follows an opener (`(`, `[`, `{`) or
///   immediately precedes a closer (`)`, `]`, `}`, `,`, `;`, `:`) —
///   and to a single space otherwise. This produces a result that
///   reads like a normally-formatted one-line signature: `fn foo(a, b)`
///   not `fn foo( a, b )`.
/// - Single-line whitespace runs (multiple spaces between tokens on the
///   same source line) are left alone, so any deliberate human
///   alignment within a single line survives.
fn join_multiline(raw: &str) -> String {
    const OPENERS: &[u8] = b"([{";
    const CLOSERS: &[u8] = b")]},;:";

    let bytes = raw.as_bytes();
    let indent_len = bytes
        .iter()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .count();
    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..indent_len]);

    let rest = &raw[indent_len..];
    let mut chars = rest.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            // Consume the whole whitespace run.
            let run_start = idx;
            let mut has_newline = c == '\n' || c == '\r';
            let mut run_end = idx + c.len_utf8();
            while let Some(&(_, nc)) = chars.peek() {
                if nc == ' ' || nc == '\t' || nc == '\n' || nc == '\r' {
                    if nc == '\n' || nc == '\r' {
                        has_newline = true;
                    }
                    let (i, ch) = chars.next().expect("peeked");
                    run_end = i + ch.len_utf8();
                } else {
                    break;
                }
            }
            if !has_newline {
                // Pure intra-line whitespace — keep verbatim.
                out.push_str(&rest[run_start..run_end]);
                continue;
            }
            // Cross-line run: pick zero-or-one space based on neighbours.
            let prev = out.as_bytes().last().copied();
            let next = rest.as_bytes().get(run_end).copied();
            let drop = match (prev, next) {
                (Some(p), _) if OPENERS.contains(&p) => true,
                (_, Some(n)) if CLOSERS.contains(&n) => true,
                (None, _) => true,
                _ => false,
            };
            if !drop {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Byte offset where the header line ends (exclusive). For definitions
/// with a body block this is one byte past the body opener (`{` or `:`)
/// so the rendered header includes that opener — matching how the
/// previous line-based renderer presented things. For body-less
/// definitions we use the end of the definition node.
fn signature_end_byte(source: &str, lang: Lang, def_node: Node) -> Option<usize> {
    if let Some(body) = direct_body(lang, def_node) {
        return Some(body_opener_end(source, lang, def_node, body));
    }
    Some(def_node.end_byte())
}

/// Resolve the body block of `def_node`, looking through one level of
/// indirection for wrapper nodes that don't expose a `body` field
/// directly (Python `decorated_definition`, JS/TS variable declarations
/// whose value is an arrow / function expression).
fn direct_body<'tree>(lang: Lang, def_node: Node<'tree>) -> Option<Node<'tree>> {
    if let Some(body) = def_node.child_by_field_name("body") {
        return Some(body);
    }
    if matches!(lang, Lang::Python)
        && let Some(inner) = def_node.child_by_field_name("definition")
        && let Some(body) = inner.child_by_field_name("body")
    {
        return Some(body);
    }
    if matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
        && let Some(body) = find_value_body(def_node)
    {
        return Some(body);
    }
    None
}

/// Compute the byte offset just past the body opener (`{` for braced
/// languages, `:` for Python). Falls back to `body.start_byte()` if we
/// can't pinpoint the opener — the resulting header is then trimmed
/// short of the brace, which is still readable.
fn body_opener_end(source: &str, lang: Lang, def_node: Node, body: Node) -> usize {
    match lang {
        Lang::Python => {
            // The `:` is an anonymous child of the class/function
            // definition, sitting between the parameter list / name
            // and the indented `block` body. Scan backwards from the
            // body's start byte for the colon — robust to whitespace
            // and comments between `:` and the indented block.
            let bytes = source.as_bytes();
            let upper = body.start_byte().min(bytes.len());
            let lower = def_node.start_byte();
            for i in (lower..upper).rev() {
                if bytes[i] == b':' {
                    return i + 1;
                }
            }
            body.start_byte()
        }
        _ => {
            // Braced languages: body starts at the `{`. Include it.
            let bytes = source.as_bytes();
            let start = body.start_byte();
            if bytes.get(start) == Some(&b'{') {
                start + 1
            } else {
                start
            }
        }
    }
}

/// Walk into a JS/TS `lexical_declaration` / `variable_declaration` /
/// `field_definition` looking for the body of an arrow / function
/// expression on the right-hand side.
fn find_value_body<'tree>(def_node: Node<'tree>) -> Option<Node<'tree>> {
    // `field_definition` / `public_field_definition` carry `value`
    // directly.
    if let Some(value) = def_node.child_by_field_name("value")
        && let Some(body) = value.child_by_field_name("body")
    {
        return Some(body);
    }
    // `lexical_declaration` / `variable_declaration` wrap one or more
    // `variable_declarator` children, each of which carries `value`.
    let mut cursor = def_node.walk();
    for child in def_node.named_children(&mut cursor) {
        if let Some(value) = child.child_by_field_name("value")
            && let Some(body) = value.child_by_field_name("body")
        {
            return Some(body);
        }
    }
    None
}

// ----- per-file dispatch -----------------------------------------------

struct FileSkeleton {
    path_str: String,
    /// Rendered body on success, or per-file error string on failure.
    /// Success bodies do *not* include the `===== … =====` header — the
    /// top-level renderer adds it.
    body: Result<String, String>,
    def_count: usize,
}

fn skeleton_one(cwd: &str, path_str: &str) -> FileSkeleton {
    let path = super::resolve_path(cwd, path_str);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    let Some(lang) = Lang::from_extension(ext) else {
        let msg = if ext.is_empty() {
            "no skeleton support for files without an extension; use `read` instead".to_string()
        } else {
            format!("no skeleton support for .{ext}; use `read` instead")
        };
        return FileSkeleton {
            path_str: path_str.to_string(),
            body: Err(msg),
            def_count: 0,
        };
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return FileSkeleton {
                path_str: path_str.to_string(),
                body: Err(format!("failed to read {}: {e}", path.display())),
                def_count: 0,
            };
        }
    };

    match extract(&source, lang) {
        Ok(headers) => {
            let count = headers.len();
            let body = if headers.is_empty() {
                "(no top-level definitions found)".to_string()
            } else {
                headers.join("\n")
            };
            FileSkeleton {
                path_str: path_str.to_string(),
                body: Ok(body),
                def_count: count,
            }
        }
        Err(e) => FileSkeleton {
            path_str: path_str.to_string(),
            body: Err(e),
            def_count: 0,
        },
    }
}

// ----- tool entrypoint --------------------------------------------------

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
            "too many paths: {} (max {MAX_PATHS})",
            paths_arr.len()
        ));
    }

    let mut paths: Vec<String> = Vec::with_capacity(paths_arr.len());
    for (i, v) in paths_arr.iter().enumerate() {
        let Some(s) = v.as_str() else {
            return ToolOutput::error(format!("paths[{i}] is not a string"));
        };
        paths.push(s.to_string());
    }

    let n_paths = paths.len();
    let mut out = String::new();
    let mut total_defs = 0usize;
    let mut errors = 0usize;
    let mut successes = 0usize;
    let mut cap_reached = false;
    let mut skipped_after_cap = 0usize;

    for (i, path_str) in paths.iter().enumerate() {
        if cap_reached {
            skipped_after_cap += 1;
            continue;
        }
        let fs = skeleton_one(cwd, path_str);
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("===== {} =====\n", fs.path_str));
        match &fs.body {
            Ok(body) => {
                out.push_str(body);
                total_defs += fs.def_count;
                successes += 1;
            }
            Err(msg) => {
                out.push_str("error: ");
                out.push_str(msg);
                errors += 1;
            }
        }
        if out.len() >= MAX_TOTAL_BYTES {
            cap_reached = true;
        }
    }
    if cap_reached && skipped_after_cap > 0 {
        out.push_str(&format!(
            "\n\n[truncated: byte cap reached, {skipped_after_cap} file(s) not skeletonised]"
        ));
    }

    let summary = if errors == 0 {
        format!("skeleton: {n_paths} file(s), {total_defs} def(s)")
    } else {
        format!(
            "skeleton: {n_paths} file(s), {total_defs} def(s), {errors} error{}",
            if errors == 1 { "" } else { "s" }
        )
    };

    let mut output = ToolOutput::text(out).with_summary(summary);
    // Partial success keeps `is_error = false`; only flag the call when
    // every requested path failed (mirrors `read`).
    output.is_error = successes == 0;
    output
}

// ----- tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tau_agent_plugin::CancelToken;

    fn write_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).expect("write test file");
        p
    }

    fn run(paths: &[&std::path::Path]) -> ToolOutput {
        let arr: Vec<String> = paths
            .iter()
            .map(|p| p.to_str().expect("utf-8 path").to_string())
            .collect();
        execute(
            serde_json::json!({"paths": arr}),
            "/tmp",
            &CancelToken::new(),
        )
    }

    fn body(out: &ToolOutput) -> String {
        out.content[0].text().to_string()
    }

    // ---- Rust ---------------------------------------------------------

    #[test]
    fn rust_skeleton_includes_top_level_and_nested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
/// A doc-commented struct.
pub struct Foo {
    pub field: i32,
}

pub fn bar(x: i32) -> i32 {
    let inner = x + 1;
    inner
}

impl Foo {
    pub fn baz(&self) -> i32 {
        self.field
    }
}

mod inner {
    pub fn qux() {
        println!(\"hi\");
    }
}

pub trait Greet {
    fn hello(&self) -> &str;
}

pub enum E {
    A,
    B,
}

pub type Alias = i32;
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);

        // Header lines we expect to see (verbatim from source).
        for needle in [
            "pub struct Foo {",
            "pub fn bar(x: i32) -> i32 {",
            "impl Foo {",
            "    pub fn baz(&self) -> i32 {",
            "mod inner {",
            "    pub fn qux() {",
            "pub trait Greet {",
            "    fn hello(&self) -> &str;",
            "pub enum E {",
            "pub type Alias = i32;",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }

        // Body lines must NOT leak.
        for forbidden in ["let inner = x + 1;", "self.field", "println!(\"hi\");"] {
            assert!(
                !text.contains(forbidden),
                "skeleton leaked body line {forbidden:?} in:\n{text}"
            );
        }
        // pub field is on its own line (not the header line for Foo) so it
        // should be excluded.
        assert!(
            !text.contains("    pub field: i32,"),
            "skeleton leaked struct field in:\n{text}"
        );
    }

    // ---- Python -------------------------------------------------------

    #[test]
    fn python_skeleton_includes_decorated_and_nested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
import os

def top_level(x):
    \"\"\"docstring\"\"\"
    return x + 1

@staticmethod
def decorated():
    return 42

class Outer:
    def method(self, y):
        z = y * 2
        return z

    class Inner:
        def inner_method(self):
            pass
";
        let p = write_file(dir.path(), "thing.py", src);
        let out = run(&[&p]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);

        for needle in [
            "def top_level(x):",
            "def decorated():",
            "class Outer:",
            "    def method(self, y):",
            "    class Inner:",
            "        def inner_method(self):",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        for forbidden in ["z = y * 2", "return z", "import os"] {
            assert!(
                !text.contains(forbidden),
                "skeleton leaked body line {forbidden:?} in:\n{text}"
            );
        }
    }

    // ---- TypeScript ---------------------------------------------------

    #[test]
    fn typescript_skeleton_includes_class_iface_and_arrow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
export interface Greeter {
  greet(name: string): string;
}

export type Id = number;

export class Hello implements Greeter {
  private prefix: string;

  greet(name: string): string {
    return this.prefix + name;
  }
}

export function plain(x: number): number {
  return x + 1;
}

export const arrow = (x: number): number => {
  return x * 2;
};
";
        let p = write_file(dir.path(), "thing.ts", src);
        let out = run(&[&p]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);

        for needle in [
            "export interface Greeter {",
            "export type Id = number;",
            "export class Hello implements Greeter {",
            "  greet(name: string): string {",
            "export function plain(x: number): number {",
            "export const arrow = (x: number): number => {",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        for forbidden in [
            "return this.prefix + name;",
            "return x + 1;",
            "return x * 2;",
        ] {
            assert!(
                !text.contains(forbidden),
                "skeleton leaked body line {forbidden:?} in:\n{text}"
            );
        }
    }

    // ---- JavaScript ---------------------------------------------------

    #[test]
    fn javascript_skeleton_includes_class_function_and_arrow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
class Foo {
  bar() {
    return 1;
  }
}

function top(x) {
  return x + 1;
}

const arrow = (y) => {
  return y * 2;
};
";
        let p = write_file(dir.path(), "thing.js", src);
        let out = run(&[&p]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);

        for needle in [
            "class Foo {",
            "  bar() {",
            "function top(x) {",
            "const arrow = (y) => {",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        for forbidden in ["return 1;", "return x + 1;", "return y * 2;"] {
            assert!(
                !text.contains(forbidden),
                "skeleton leaked body line {forbidden:?} in:\n{text}"
            );
        }
    }

    // ---- error / mixed cases -----------------------------------------

    #[test]
    fn unsupported_ext_returns_per_file_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = write_file(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let rs = write_file(dir.path(), "lib.rs", "pub fn ok() {}\n");
        let out = run(&[&cfg, &rs]);
        // Mixed success → call is NOT an error.
        assert!(!out.is_error, "mixed success should not flag is_error");
        let text = body(&out);
        assert!(text.contains("error: no skeleton support for .toml"));
        assert!(text.contains("pub fn ok() {}"));
    }

    #[test]
    fn all_unsupported_returns_is_error_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.toml", "x=1\n");
        let b = write_file(dir.path(), "b.toml", "y=2\n");
        let out = run(&[&a, &b]);
        assert!(out.is_error, "all-failed must be flagged");
        let text = body(&out);
        assert!(text.contains("no skeleton support for .toml"));
    }

    #[test]
    fn missing_file_partial_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = write_file(dir.path(), "lib.rs", "pub fn alive() {}\n");
        let missing = dir.path().join("ghost.rs");
        let out = run(&[&real, &missing]);
        assert!(!out.is_error, "partial success should not flag is_error");
        let text = body(&out);
        assert!(text.contains("pub fn alive() {}"));
        assert!(text.contains("error: failed to read"));
    }

    #[test]
    fn empty_paths_rejected() {
        let out = execute(
            serde_json::json!({"paths": []}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("empty"));
    }

    #[test]
    fn missing_paths_argument_rejected() {
        let out = execute(serde_json::json!({}), "/tmp", &CancelToken::new());
        assert!(out.is_error);
        assert!(body(&out).contains("paths"));
    }

    #[test]
    fn non_string_path_entry_rejected() {
        let out = execute(
            serde_json::json!({"paths": ["ok.rs", 42]}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("paths[1]"));
    }

    #[test]
    fn too_many_paths_rejected() {
        let many: Vec<String> = (0..(MAX_PATHS + 1)).map(|i| format!("f{i}.rs")).collect();
        let out = execute(
            serde_json::json!({"paths": many}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("too many paths"));
    }

    #[test]
    fn header_line_format_for_two_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.rs", "pub fn one() {}\n");
        let b = write_file(dir.path(), "b.rs", "pub fn two() {}\n");
        let out = run(&[&a, &b]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains(&format!("===== {} =====", a.to_str().expect("utf-8"))));
        assert!(text.contains(&format!("===== {} =====", b.to_str().expect("utf-8"))));
        assert!(text.contains("pub fn one() {}"));
        assert!(text.contains("pub fn two() {}"));
        let summary = out.summary.expect("summary");
        assert!(summary.contains("2 file(s)"), "got: {summary}");
        assert!(summary.contains("2 def(s)"), "got: {summary}");
    }

    #[test]
    fn empty_source_file_renders_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write_file(dir.path(), "empty.rs", "");
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("(no top-level definitions found)"));
    }

    #[test]
    fn nested_definitions_preserved_rust_impl() {
        // Explicit regression coverage for the nested-method case called
        // out in the spec.
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub struct A;

impl A {
    pub fn first(&self) -> i32 { 1 }
    pub fn second(&self) -> i32 { 2 }
}
";
        let p = write_file(dir.path(), "a.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("    pub fn first(&self) -> i32 { 1 }"));
        assert!(text.contains("    pub fn second(&self) -> i32 { 2 }"));
    }

    #[test]
    fn definitions_emitted_in_source_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub fn alpha() {}
pub fn bravo() {}
pub fn charlie() {}
";
        let p = write_file(dir.path(), "x.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        let a = text.find("alpha").expect("alpha present");
        let b = text.find("bravo").expect("bravo present");
        let c = text.find("charlie").expect("charlie present");
        assert!(a < b && b < c, "out of order:\n{text}");
    }

    // ---- multi-line signature joining --------------------------------

    #[test]
    fn rust_multi_line_function_signature_joined() {
        // Mirrors the spec's `execute_tool` example: a top-level fn
        // whose parameter list and return type wrap across several
        // lines should collapse to one logical line in the skeleton.
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub fn execute_tool(
    tools: &[ToolDef],
    tool_call: &ToolCall,
    cwd: &str,
    cancel: &CancelToken,
) -> ToolResultMessage {
    todo!()
}

pub fn one_liner() -> i32 { 7 }
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(
            text.contains(
                "pub fn execute_tool(tools: &[ToolDef], tool_call: &ToolCall, cwd: &str, cancel: &CancelToken,) -> ToolResultMessage {"
            ),
            "missing joined signature in:\n{text}"
        );
        // Single-line signature must remain byte-for-byte unchanged.
        assert!(
            text.contains("pub fn one_liner() -> i32 { 7 }"),
            "single-line signature corrupted in:\n{text}"
        );
        // The body line must not leak.
        assert!(
            !text.contains("todo!()"),
            "skeleton leaked body in:\n{text}"
        );
    }

    #[test]
    fn rust_multi_line_method_signature_joined_inside_impl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
impl Foo {
    pub fn long_method(
        &self,
        arg_one: i32,
        arg_two: &str,
    ) -> Result<(), Error> {
        Ok(())
    }
}
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(
            text.contains(
                "    pub fn long_method(&self, arg_one: i32, arg_two: &str,) -> Result<(), Error> {"
            ),
            "missing joined method signature in:\n{text}"
        );
        assert!(
            !text.contains("Ok(())"),
            "skeleton leaked method body in:\n{text}"
        );
    }

    #[test]
    fn rust_multi_line_trait_method_signature_joined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub trait Big {
    fn wrapped(
        &self,
        x: i32,
    ) -> i32;
}
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(
            text.contains("    fn wrapped(&self, x: i32,) -> i32;"),
            "missing joined trait method sig in:\n{text}"
        );
    }

    #[test]
    fn python_multi_line_def_signature_joined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
def wide(
    a,
    b,
    c,
):
    return a + b + c

class C:
    def m(
        self,
        x,
    ):
        return x
";
        let p = write_file(dir.path(), "thing.py", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(
            text.contains("def wide(a, b, c,):"),
            "missing joined def in:\n{text}"
        );
        assert!(
            text.contains("    def m(self, x,):"),
            "missing joined method in:\n{text}"
        );
        assert!(
            !text.contains("return a + b + c"),
            "skeleton leaked body in:\n{text}"
        );
    }

    #[test]
    fn typescript_multi_line_function_and_method_joined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
export function wide(
  a: number,
  b: number,
): number {
  return a + b;
}

export class K {
  long(
    x: number,
    y: number,
  ): number {
    return x + y;
  }
}

export function tight(z: number): number { return z; }
";
        let p = write_file(dir.path(), "thing.ts", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(
            text.contains("export function wide(a: number, b: number,): number {"),
            "missing joined fn in:\n{text}"
        );
        assert!(
            text.contains("  long(x: number, y: number,): number {"),
            "missing joined method in:\n{text}"
        );
        // Single-line stays untouched.
        assert!(
            text.contains("export function tight(z: number): number { return z; }"),
            "single-line sig corrupted in:\n{text}"
        );
        assert!(
            !text.contains("return a + b;"),
            "skeleton leaked body in:\n{text}"
        );
    }

    #[test]
    fn javascript_multi_line_function_joined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
function wide(
  a,
  b,
  c,
) {
  return a + b + c;
}

function tight(x) { return x; }
";
        let p = write_file(dir.path(), "thing.js", src);
        let out = run(&[&p]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(
            text.contains("function wide(a, b, c,) {"),
            "missing joined fn in:\n{text}"
        );
        assert!(
            text.contains("function tight(x) { return x; }"),
            "single-line sig corrupted in:\n{text}"
        );
    }

    #[test]
    fn smoke_test_execute_tool_signature_in_worker_crate() {
        // The spec calls out `execute_tool` in this crate's
        // `tools/mod.rs` as the canonical multi-line signature that
        // should now collapse onto one virtual line. Re-skeletonise it
        // and assert the joined header is present — this guards both
        // the join logic and the `definition.*` query plumbing.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tools")
            .join("mod.rs");
        // If the file ever moves we want the test to flag it loudly
        // rather than silently no-op.
        assert!(path.is_file(), "missing source file: {}", path.display());
        let out = run(&[&path]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(
            text.contains(
                "pub fn execute_tool(tools: &[ToolDef], tool_call: &ToolCall, cwd: &str, cancel: &CancelToken,) -> ToolResultMessage {"
            ),
            "missing joined `execute_tool` signature in:\n{text}"
        );
    }
}
