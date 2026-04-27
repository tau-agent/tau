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

use super::{ToolDef, ToolOutput};
use std::collections::BTreeSet;
use tau_agent_plugin::Tool;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

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

/// Languages supported in v1. Each variant maps to a tree-sitter grammar
/// + a query string of `@name.definition.*` captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
}

impl Lang {
    fn from_ext(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    fn query_src(self) -> &'static str {
        match self {
            Self::Rust => RUST_QUERY,
            Self::Python => PYTHON_QUERY,
            Self::JavaScript => JAVASCRIPT_QUERY,
            Self::TypeScript | Self::Tsx => TYPESCRIPT_QUERY,
        }
    }
}

// ----- queries ---------------------------------------------------------
//
// Ported from Dirac's `src/services/tree-sitter/queries/{rust,python,
// typescript,javascript}.ts`. We keep only the `@name.definition.*`
// captures (and their accompanying `@doc` predicates) — the
// `@name.reference` clauses for cross-references aren't needed for the
// outline view and just slow the cursor down.

const RUST_QUERY: &str = r#"
;; Modules
(
  [(line_comment) (block_comment)]* @doc
  .
  (mod_item
    name: (identifier) @name.definition.module) @definition.module
)

;; Structs
(
  [(line_comment) (block_comment)]* @doc
  .
  (struct_item
    name: (type_identifier) @name.definition.class) @definition.class
)

;; Enums
(
  [(line_comment) (block_comment)]* @doc
  .
  (enum_item
    name: (type_identifier) @name.definition.class) @definition.class
)

;; Traits
(
  [(line_comment) (block_comment)]* @doc
  .
  (trait_item
    name: (type_identifier) @name.definition.interface) @definition.interface
)

;; Type aliases
(
  [(line_comment) (block_comment)]* @doc
  .
  (type_item
    name: (type_identifier) @name.definition.type) @definition.type
)

;; Free functions
(
  [(line_comment) (block_comment)]* @doc
  .
  (function_item
    name: (identifier) @name.definition.function) @definition.function
)

;; Function signatures (e.g. trait methods without a body, extern fn decls)
(
  [(line_comment) (block_comment)]* @doc
  .
  (function_signature_item
    name: (identifier) @name.definition.function) @definition.function
)

;; Impl blocks (so the `impl Foo for Bar` line shows up as a header)
(impl_item) @definition.impl

;; Methods (functions inside impl blocks)
(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @name.definition.method) @definition.method))

;; Trait methods (with body)
(trait_item
  body: (declaration_list
    (function_item
      name: (identifier) @name.definition.method) @definition.method))

;; Trait method signatures (no body)
(trait_item
  body: (declaration_list
    (function_signature_item
      name: (identifier) @name.definition.method) @definition.method))

;; Closures assigned to variables
(let_declaration
  pattern: (identifier) @name.definition.function
  value: (closure_expression)) @definition.function
"#;

const PYTHON_QUERY: &str = r#"
;; Classes
(class_definition
  name: (identifier) @name.definition.class) @definition.class

;; Methods (functions inside classes)
(class_definition
  body: (block
    (function_definition
      name: (identifier) @name.definition.method) @definition.method))

;; Top-level functions
(function_definition
  name: (identifier) @name.definition.function) @definition.function

;; Decorated definitions
(decorated_definition
  definition: [
    (class_definition
      name: (identifier) @name.definition.class)
    (function_definition
      name: (identifier) @name.definition.function)
  ]) @definition.symbol

;; Lambdas assigned to variables
(assignment
  left: (identifier) @name.definition.function
  right: (lambda)) @definition.function
"#;

const TYPESCRIPT_QUERY: &str = r#"
(function_signature
  name: (identifier) @name.definition.function) @definition.function

(function_declaration
  name: (identifier) @name.definition.function) @definition.function

(method_signature
  name: [(property_identifier) (identifier)] @name.definition.method) @definition.method

(method_definition
  name: [(property_identifier) (identifier)] @name.definition.method) @definition.method

(abstract_method_signature
  name: [(property_identifier) (identifier)] @name.definition.method) @definition.method

(abstract_class_declaration
  name: (type_identifier) @name.definition.class) @definition.class

(class_declaration
  name: (type_identifier) @name.definition.class) @definition.class

(module
  name: [(identifier) (string)] @name.definition.module) @definition.module

(interface_declaration
  name: (type_identifier) @name.definition.interface) @definition.interface

(enum_declaration
  name: (identifier) @name.definition.enum) @definition.enum

(type_alias_declaration
  name: (type_identifier) @name.definition.type) @definition.type

;; Variable declarations with arrow functions or function expressions
(lexical_declaration
  (variable_declarator
    name: (identifier) @name.definition.function
    value: [(arrow_function) (function_expression)])) @definition.function

(variable_declaration
  (variable_declarator
    name: (identifier) @name.definition.function
    value: [(arrow_function) (function_expression)])) @definition.function

;; Class properties with arrow functions
(public_field_definition
  name: [(property_identifier) (identifier)] @name.definition.method
  value: [(arrow_function) (function_expression)]) @definition.method
"#;

const JAVASCRIPT_QUERY: &str = r#"
(method_definition
  name: [(property_identifier) (identifier)] @name.definition.method) @definition.method

[
  (class
    name: (_) @name.definition.class)
  (class_declaration
    name: (_) @name.definition.class)
] @definition.class

[
  (function_declaration
    name: (identifier) @name.definition.function)
  (generator_function_declaration
    name: (identifier) @name.definition.function)
] @definition.function

;; Class fields with arrow functions
(field_definition
  property: (property_identifier) @name.definition.method
  value: [(arrow_function) (function_expression)]) @definition.method

;; Variable declarations with arrow functions / function expressions
[
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name.definition.function
      value: [(arrow_function) (function_expression)]))
  (variable_declaration
    (variable_declarator
      name: (identifier) @name.definition.function
      value: [(arrow_function) (function_expression)]))
] @definition.function
"#;

// ----- extraction ------------------------------------------------------

/// Parse `source` with `lang`'s grammar and return the *header lines* of
/// every definition captured by the query, sorted by source position and
/// deduped by start row.
///
/// We intentionally emit the source line containing each captured name
/// node rather than reconstructing a trimmed signature. That preserves
/// indentation, attributes/decorators on the same line, `pub`, `async`,
/// generics, return types, etc. — high-signal context for ~one line of
/// output per definition.
fn extract(source: &str, lang: Lang) -> Result<Vec<String>, String> {
    let language = lang.language();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| format!("set_language: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter parse failed".to_string())?;
    let query =
        Query::new(&language, lang.query_src()).map_err(|e| format!("query compile: {e}"))?;

    let mut cursor = QueryCursor::new();
    let mut rows: BTreeSet<usize> = BTreeSet::new();
    let lines: Vec<&str> = source.lines().collect();
    let capture_names = query.capture_names();

    let mut it = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = it.next() {
        for cap in m.captures {
            let name = capture_names
                .get(cap.index as usize)
                .copied()
                .unwrap_or_default();
            // Two kinds of captures contribute a row:
            // 1. `@name.definition.*` — the identifier; its row is the
            //    signature line.
            // 2. `@definition.impl` — the whole `impl …` block; we want
            //    its first row (the `impl Foo for Bar {` line). This is
            //    Rust-specific since impls don't have a `name:` child.
            if name.starts_with("name.definition") || name == "definition.impl" {
                rows.insert(cap.node.start_position().row);
            }
        }
    }

    Ok(rows
        .into_iter()
        .filter_map(|r| lines.get(r).map(|s| s.to_string()))
        .collect())
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

    let Some(lang) = Lang::from_ext(ext) else {
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
}
