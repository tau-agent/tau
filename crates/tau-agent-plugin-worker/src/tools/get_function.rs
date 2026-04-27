//! `get_function` tool — extract the complete source slice of named
//! functions / methods from one or more files using tree-sitter.
//!
//! Pairs with `get_file_skeleton` (`tools/skeleton.rs`): skeleton to skim,
//! `get_function` to drill into the bodies you actually want. Inspired by
//! Dirac's `getFunctions` (`src/utils/ASTAnchorBridge.ts`).
//!
//! ## Schema
//!
//! ```json
//! {"paths": ["src/lib.rs"], "function_names": ["Foo.bar", "frobnicate"]}
//! ```
//!
//! All-to-all: every (path, name) pair is attempted. Misses are reported
//! in a footer rather than failing the call. The call is only flagged
//! `is_error = true` if **no** name was extracted from **any** file (i.e.
//! the call produced zero useful output).
//!
//! ## Name matching
//!
//! The canonical method-path syntax is dot-separated (`Foo.bar`,
//! `Outer.Inner.method`). Rust's `::` is accepted as an alias and
//! normalised to `.` before matching. A request `bar` matches any
//! definition whose normalised full name **ends with** `.bar` (or equals
//! `bar`); a request `Foo.bar` requires the suffix `Foo.bar`. Ambiguous
//! requests return every match — the model can disambiguate by reading
//! the output.
//!
//! ## Extended range
//!
//! Each emitted body covers more than the bare `fn` / `def` / `function`
//! node — we walk up through wrapper nodes (`export_statement`,
//! `decorated_definition`, `internal_module`, …) and back across
//! preceding doc comments / decorators / attributes (`///`, `#[derive]`,
//! `@decorator`). That means the slice begins at the first relevant doc
//! line and ends at the closing brace of the body, byte-identical to the
//! source.

use super::tree_sitter_support::{self, Lang};
use super::{ToolDef, ToolOutput};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use tau_agent_plugin::Tool;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

/// Maximum number of paths accepted in one call. Mirrors `read` and
/// `get_file_skeleton` so the model has a consistent budget.
pub const MAX_PATHS: usize = 20;

/// Maximum number of function names per call. We keep this fairly
/// generous — the all-to-all cross product still bounds work to
/// `paths × names` per call.
pub const MAX_NAMES: usize = 32;

/// Total output byte cap. Function bodies are usually small, but a single
/// 5k-line generated `match` arm can blow up; cap defensively.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "get_function".into(),
            description:
                "Return the complete source bodies of named functions or methods from one or more files (tree-sitter). Pairs with get_file_skeleton: skim the skeleton, then pull only the bodies you need instead of `read`-ing whole files. Method names use dot-paths (`Foo.bar`); `::` is also accepted for Rust ergonomics. Bare names (`bar`) match any definition whose qualified name ends in `.bar`. Supports Rust, Python, JavaScript, TypeScript, and TSX."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_PATHS,
                        "description": "Paths to source files to search (1–20)."
                    },
                    "function_names": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_NAMES,
                        "description": "Names to extract. Methods may be qualified with `.` (`Foo.bar`) or `::` (Rust)."
                    }
                },
                "required": ["paths", "function_names"]
            }),
        },
        execute: Box::new(execute),
        prepare_arguments: None,
    }
}

// ----- name normalisation ---------------------------------------------

/// Canonicalise a name path: replace `::` with `.` and trim whitespace.
/// Empty inputs return an empty string (caller decides whether to reject).
fn normalise(name: &str) -> String {
    name.trim().replace("::", ".")
}

/// Does `full` (already normalised) satisfy a request for `req` (already
/// normalised)? Either an exact match or a dotted suffix match.
fn name_matches(full: &str, req: &str) -> bool {
    if full == req {
        return true;
    }
    if let Some(stripped) = full.strip_suffix(req) {
        // Suffix match must be at a `.` boundary so requesting `bar`
        // doesn't match `foobar`.
        return stripped.ends_with('.');
    }
    false
}

// ----- definition extraction ------------------------------------------

/// One captured definition with everything `get_function` needs:
/// the wrapping node (used for ancestor walks), its bare name, and the
/// resolved dotted full name.
#[derive(Debug, Clone)]
struct Def<'tree> {
    /// The captured `definition.*` node (function / method / class).
    def_node: Node<'tree>,
    /// The captured `name.definition.*` node.
    name_node: Node<'tree>,
    /// Bare identifier text (e.g. `bar`).
    bare_name: String,
}

/// Run the language's tag query and collect every match that has both a
/// `name.definition.*` capture and a `definition.*` capture. Captures
/// without both are ignored — they're typically the `@doc` lead-in
/// comments which aren't standalone definitions.
fn collect_defs<'tree>(
    tree: &'tree tree_sitter::Tree,
    source: &str,
    lang: Lang,
) -> Vec<Def<'tree>> {
    let query = tree_sitter_support::query_for(lang);
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut defs: Vec<Def<'tree>> = Vec::new();

    let mut it = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = it.next() {
        let mut name_node: Option<Node<'tree>> = None;
        let mut def_node: Option<Node<'tree>> = None;
        for cap in m.captures {
            let cname = capture_names
                .get(cap.index as usize)
                .copied()
                .unwrap_or_default();
            if cname.starts_with("name.definition.") {
                // Prefer the first name capture in the match — multiple
                // `name.*` captures in one match are unusual.
                name_node.get_or_insert(cap.node);
            } else if cname.starts_with("definition.") {
                def_node.get_or_insert(cap.node);
            }
        }
        if let (Some(name_node), Some(def_node)) = (name_node, def_node) {
            let bare_name = source[name_node.byte_range()].to_string();
            defs.push(Def {
                def_node,
                name_node,
                bare_name,
            });
        }
    }

    defs
}

/// Resolve a `Def` to its dotted full name by walking the parent chain.
///
/// For each ancestor of `def.def_node` that is itself a captured
/// definition (i.e. its node id appears in `def_by_node`), prepend that
/// definition's bare name with a `.` separator. Stops at the root or at
/// the first ancestor that isn't a captured definition above a captured
/// definition (we don't skip non-def ancestors — that would let unrelated
/// lexical scopes leak in).
///
/// We DO skip purely-syntactic wrappers — `block`, `declaration_list`,
/// `decorated_definition`, etc. — because they sit between e.g. a Python
/// `class_definition` and the `function_definition` for its method, and
/// are never themselves captured as `definition.*`.
fn resolve_full_name<'tree>(def: &Def<'tree>, def_by_node: &HashMap<usize, &Def<'tree>>) -> String {
    let mut parts: Vec<String> = vec![def.bare_name.clone()];
    let mut cursor = def.def_node.parent();
    let mut visited: BTreeSet<usize> = BTreeSet::new();
    visited.insert(def.def_node.id());
    while let Some(node) = cursor {
        if !visited.insert(node.id()) {
            break;
        }
        if let Some(parent_def) = def_by_node.get(&node.id())
            && parent_def.name_node.id() != def.name_node.id()
        {
            parts.push(parent_def.bare_name.clone());
        }
        cursor = node.parent();
    }
    parts.reverse();
    parts.join(".")
}

// ----- extended range walk --------------------------------------------

/// Wrapper kinds we walk *up* through to capture export / decorator /
/// ambient-namespace prefixes. Mirrors Dirac's allowlist.
const WRAPPER_KINDS: &[&str] = &[
    "export_statement",
    "export_declaration",
    "ambient_declaration",
    "decorated_definition",
    "internal_module",
];

/// Sibling kinds we walk *back* over to absorb leading doc comments,
/// decorators, and attributes. Mirrors Dirac's allowlist plus Rust's
/// outer attribute node.
const LEADING_SIBLING_KINDS: &[&str] = &[
    "comment",
    "line_comment",
    "block_comment",
    "decorator",
    "attribute",
    "attribute_item",
    "inner_attribute_item",
];

/// Compute the byte range to emit for `def`, expanding around the bare
/// definition node:
///
/// 1. Walk up while the parent is a known wrapper. This pulls in
///    `export function`, Python `@decorator`-decorated definitions, etc.
/// 2. From that wrapper-expanded node, walk back across previous *named*
///    siblings while they're comment-like / decorator / attribute. Each
///    step extends the start byte upward — that's how `///` doc lines and
///    `#[inline]` attributes get pulled in even when separated by blank
///    lines (tree-sitter elides whitespace from named-sibling walks).
fn extended_range(def_node: Node<'_>) -> (usize, usize) {
    // 1. Walk through wrappers.
    let mut top = def_node;
    while let Some(parent) = top.parent() {
        if WRAPPER_KINDS.contains(&parent.kind()) {
            top = parent;
        } else {
            break;
        }
    }

    let mut start = top.start_byte();
    let end = top.end_byte();

    // 2. Walk back over leading doc / decorator / attribute siblings.
    let mut cursor = top.prev_named_sibling();
    while let Some(sib) = cursor {
        if LEADING_SIBLING_KINDS.contains(&sib.kind()) {
            start = sib.start_byte();
            cursor = sib.prev_named_sibling();
        } else {
            break;
        }
    }

    (start, end)
}

// ----- per-file pipeline ----------------------------------------------

/// One emitted hit, ready for rendering.
struct Hit {
    /// Path string as it appeared in the request (preserves user input).
    path_str: String,
    /// Resolved dotted full name (e.g. `Foo.bar`).
    full_name: String,
    /// 1-indexed start line — for headers; the body is byte-sliced.
    start_line: usize,
    /// Verbatim source slice covering the extended range.
    body: String,
    /// Byte range within the source — used for cross-file dedup keys.
    byte_range: (usize, usize),
}

struct FileResult {
    /// Either rendered hits + which requests matched at least once, or a
    /// per-file error string.
    outcome: Result<(Vec<Hit>, BTreeSet<String>), String>,
}

fn process_file(cwd: &str, path_str: &str, requests_norm: &[String]) -> FileResult {
    let path = super::resolve_path(cwd, path_str);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    let Some(lang) = Lang::from_extension(ext) else {
        let msg = if ext.is_empty() {
            "no get_function support for files without an extension; use `read` instead".to_string()
        } else {
            format!("no get_function support for .{ext}; use `read` instead")
        };
        return FileResult { outcome: Err(msg) };
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return FileResult {
                outcome: Err(format!("failed to read {}: {e}", path.display())),
            };
        }
    };

    let tree = match tree_sitter_support::parse(lang, &source) {
        Ok(t) => t,
        Err(e) => {
            return FileResult {
                outcome: Err(format!("parse error: {e}")),
            };
        }
    };

    let defs = collect_defs(&tree, &source, lang);
    // Index defs by their `def_node` id so the parent walk can ask "is
    // this ancestor itself a captured definition?" in O(1).
    let mut def_by_node: HashMap<usize, &Def<'_>> = HashMap::with_capacity(defs.len());
    for d in &defs {
        def_by_node.insert(d.def_node.id(), d);
    }

    let mut hits: Vec<Hit> = Vec::new();
    let mut matched_reqs: BTreeSet<String> = BTreeSet::new();
    let mut seen_ranges: BTreeSet<(usize, usize)> = BTreeSet::new();

    for d in &defs {
        let full_name = resolve_full_name(d, &def_by_node);
        let normalised = normalise(&full_name);
        for req in requests_norm {
            if name_matches(&normalised, req) {
                matched_reqs.insert(req.clone());
                let (start, end) = extended_range(d.def_node);
                if !seen_ranges.insert((start, end)) {
                    // Already emitted (different request resolved to the
                    // same definition); skip.
                    continue;
                }
                let body = source
                    .get(start..end)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let start_line = d.def_node.start_position().row + 1;
                hits.push(Hit {
                    path_str: path_str.to_string(),
                    full_name: full_name.clone(),
                    start_line,
                    body,
                    byte_range: (start, end),
                });
            }
        }
    }

    // Emit hits in source order so the output reads top-to-bottom of the
    // file regardless of which request triggered each one.
    hits.sort_by_key(|h| h.byte_range.0);

    FileResult {
        outcome: Ok((hits, matched_reqs)),
    }
}

// ----- rendering -------------------------------------------------------

/// Format one hit as a section.
fn render_hit(h: &Hit) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{}::{} (line {})\n--\n",
        display_path(&h.path_str),
        h.full_name,
        h.start_line
    ));
    s.push_str(&h.body);
    if !h.body.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Trim a path for display: drop leading `./` and absolute-path prefixes
/// when they collapse to something short, but keep things straightforward
/// — just hand back what the user passed in. (The tool reproduces user
/// input verbatim, mirroring how `read` echoes paths.)
fn display_path(path_str: &str) -> String {
    Path::new(path_str).to_string_lossy().to_string()
}

// ----- tool entrypoint -------------------------------------------------

fn execute(
    args: serde_json::Value,
    cwd: &str,
    _cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    // ----- parse + validate args
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

    let Some(names_val) = args.get("function_names") else {
        return ToolOutput::error(
            "missing 'function_names' argument (expected an array of strings)",
        );
    };
    let Some(names_arr) = names_val.as_array() else {
        return ToolOutput::error("'function_names' must be an array of strings");
    };
    if names_arr.is_empty() {
        return ToolOutput::error("'function_names' array is empty — provide at least one name");
    }
    if names_arr.len() > MAX_NAMES {
        return ToolOutput::error(format!(
            "too many function_names: {} (max {MAX_NAMES})",
            names_arr.len()
        ));
    }

    let mut paths: Vec<String> = Vec::with_capacity(paths_arr.len());
    for (i, v) in paths_arr.iter().enumerate() {
        let Some(s) = v.as_str() else {
            return ToolOutput::error(format!("paths[{i}] is not a string"));
        };
        paths.push(s.to_string());
    }

    let mut requested_raw: Vec<String> = Vec::with_capacity(names_arr.len());
    let mut requested_norm: Vec<String> = Vec::with_capacity(names_arr.len());
    for (i, v) in names_arr.iter().enumerate() {
        let Some(s) = v.as_str() else {
            return ToolOutput::error(format!("function_names[{i}] is not a string"));
        };
        let norm = normalise(s);
        if norm.is_empty() {
            return ToolOutput::error(format!("function_names[{i}] is empty after trimming"));
        }
        requested_raw.push(s.to_string());
        requested_norm.push(norm);
    }

    // ----- per-file processing
    let mut out = String::new();
    let mut total_hits = 0usize;
    let mut file_errors = 0usize;
    let mut file_successes = 0usize;
    let mut globally_matched: BTreeSet<String> = BTreeSet::new();
    let mut cap_reached = false;
    let mut skipped_after_cap = 0usize;

    for path_str in &paths {
        if cap_reached {
            skipped_after_cap += 1;
            continue;
        }
        let res = process_file(cwd, path_str, &requested_norm);
        match res.outcome {
            Ok((hits, matched)) => {
                file_successes += 1;
                globally_matched.extend(matched);
                if hits.is_empty() {
                    // Nothing matched in this file; stay silent (the
                    // miss footer below covers it). Avoids noisy
                    // per-file headers when most files have nothing.
                    continue;
                }
                if !out.is_empty() {
                    out.push_str("\n====================\n\n");
                }
                for (i, h) in hits.iter().enumerate() {
                    if i > 0 {
                        out.push_str("\n---\n\n");
                    }
                    out.push_str(&render_hit(h));
                    total_hits += 1;
                    if out.len() >= MAX_TOTAL_BYTES {
                        cap_reached = true;
                        break;
                    }
                }
            }
            Err(msg) => {
                file_errors += 1;
                if !out.is_empty() {
                    out.push_str("\n====================\n\n");
                }
                out.push_str(&format!("{path_str}\n--\nerror: {msg}\n"));
            }
        }
    }

    if cap_reached && skipped_after_cap > 0 {
        out.push_str(&format!(
            "\n[truncated: byte cap reached, {skipped_after_cap} file(s) not searched]\n"
        ));
    }

    // ----- miss footer
    let misses: Vec<&str> = requested_norm
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            if globally_matched.contains(n) {
                None
            } else {
                // Use the raw (un-normalised) form in the user-facing
                // message so it round-trips with what they asked for.
                Some(requested_raw[i].as_str())
            }
        })
        .collect();
    if !misses.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "Note: not found in any provided file: {}\n",
            misses.join(", ")
        ));
    }

    // ----- summary + classification
    let n_paths = paths.len();
    let n_names = requested_norm.len();
    let summary = format!(
        "get_function: {total_hits} hit(s) for {}/{} name(s) in {n_paths} file(s){}",
        globally_matched.len(),
        n_names,
        if file_errors == 0 {
            String::new()
        } else {
            format!(", {file_errors} file error(s)")
        }
    );

    let mut output = ToolOutput::text(if out.is_empty() {
        // No content at all (no hits, no errors emitted, no misses
        // either). Shouldn't normally happen — at minimum the miss
        // footer triggers when nothing is found — but keep a sensible
        // fallback.
        "(no matches)".to_string()
    } else {
        out
    })
    .with_summary(summary);

    // Soft misses don't poison the call. is_error fires when there's
    // genuinely nothing to consume: every file errored out AND we
    // produced no hits. (If at least one file parsed cleanly but found
    // no names, that's a successful call with a miss footer.)
    output.is_error = total_hits == 0 && file_successes == 0;
    output
}

// ----- tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tau_agent_plugin::CancelToken;

    fn write_file(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).expect("write test file");
        p
    }

    fn run(paths: &[&Path], names: &[&str]) -> ToolOutput {
        let arr_paths: Vec<String> = paths
            .iter()
            .map(|p| p.to_str().expect("utf-8 path").to_string())
            .collect();
        let arr_names: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
        execute(
            serde_json::json!({"paths": arr_paths, "function_names": arr_names}),
            "/tmp",
            &CancelToken::new(),
        )
    }

    fn body(out: &ToolOutput) -> String {
        out.content[0].text().to_string()
    }

    // ---- top-level functions ----------------------------------------

    #[test]
    fn rust_top_level_function_returned_without_neighbours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub fn alpha() -> i32 {
    let x = 1;
    x + 1
}

pub fn beta() -> i32 {
    let y = 2;
    y * 2
}
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p], &["alpha"]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(text.contains("pub fn alpha() -> i32 {"));
        assert!(text.contains("    x + 1"));
        assert!(!text.contains("pub fn beta"), "leaked beta:\n{text}");
        assert!(!text.contains("y * 2"), "leaked beta body:\n{text}");
    }

    // ---- impl methods + dot-path / :: alias --------------------------

    #[test]
    fn rust_impl_method_resolves_via_dot_and_colon_and_bare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub struct Foo { x: i32 }

impl Foo {
    pub fn bar(&self) -> i32 {
        self.x + 1
    }
}
";
        let p = write_file(dir.path(), "lib.rs", src);

        for name in ["Foo.bar", "Foo::bar", "bar"] {
            let out = run(&[&p], &[name]);
            assert!(!out.is_error, "out for {name}: {:?}", out.content);
            let text = body(&out);
            assert!(
                text.contains("pub fn bar(&self) -> i32 {"),
                "missing bar body for {name} in:\n{text}"
            );
            assert!(
                text.contains("Foo.bar"),
                "expected fully-qualified Foo.bar header for {name} in:\n{text}"
            );
            assert!(
                text.contains("self.x + 1"),
                "missing body line for {name} in:\n{text}"
            );
        }
    }

    // ---- Python class methods ---------------------------------------

    #[test]
    fn python_class_method_resolves_qualified_and_bare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
class Foo:
    def calculate(self, n):
        total = 0
        for i in range(n):
            total += i
        return total

    def other(self):
        return 42
";
        let p = write_file(dir.path(), "thing.py", src);

        for name in ["Foo.calculate", "calculate"] {
            let out = run(&[&p], &[name]);
            assert!(!out.is_error, "out for {name}: {:?}", out.content);
            let text = body(&out);
            assert!(
                text.contains("def calculate(self, n):"),
                "missing calculate signature for {name} in:\n{text}"
            );
            assert!(
                text.contains("total += i"),
                "missing body for {name} in:\n{text}"
            );
            assert!(
                !text.contains("def other"),
                "leaked other() for {name}:\n{text}"
            );
        }
    }

    // ---- nested / inner functions -----------------------------------

    #[test]
    fn python_nested_inner_function_reachable_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
def outer():
    x = 1
    def inner():
        return x + 1
    return inner()
";
        let p = write_file(dir.path(), "nest.py", src);
        let out = run(&[&p], &["inner"]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(
            text.contains("def inner():"),
            "missing inner signature in:\n{text}"
        );
        assert!(
            text.contains("return x + 1"),
            "missing inner body in:\n{text}"
        );
        // Outer body lines should not be in the slice (inner only).
        assert!(
            !text.contains("    x = 1"),
            "leaked outer scope into inner slice:\n{text}"
        );
    }

    // ---- ambiguous bare name returns all matches --------------------

    #[test]
    fn rust_ambiguous_bare_name_returns_all_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
pub struct Foo;
pub struct Bar;

impl Foo {
    pub fn new() -> Self { Foo }
}

impl Bar {
    pub fn new() -> Self { Bar }
}
";
        let p = write_file(dir.path(), "lib.rs", src);

        // Bare `new` matches both impls.
        let out = run(&[&p], &["new"]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("Foo.new"), "missing Foo.new:\n{text}");
        assert!(text.contains("Bar.new"), "missing Bar.new:\n{text}");
        assert!(text.contains("\n---\n"), "missing section divider:\n{text}");

        // Qualified `Foo.new` matches exactly one.
        let out = run(&[&p], &["Foo.new"]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("Foo.new"));
        assert!(
            !text.contains("Bar.new"),
            "Foo.new should not match Bar.new:\n{text}"
        );
    }

    // ---- doc comments + attributes preserved ------------------------

    #[test]
    fn rust_doc_comments_and_attributes_pulled_into_extended_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Note the blank lines between doc comment, attribute, and fn —
        // tree-sitter's prev_named_sibling skips whitespace, so the
        // extended-range walk should still pull both in.
        let src = "\
pub fn before() {}

/// Adds two numbers.
///
/// Long description here.

#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn after() {}
";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p], &["add"]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(
            text.contains("/// Adds two numbers."),
            "missing doc comment in extended range:\n{text}"
        );
        assert!(
            text.contains("/// Long description here."),
            "missing second doc line:\n{text}"
        );
        assert!(
            text.contains("#[inline]"),
            "missing inline attribute:\n{text}"
        );
        assert!(text.contains("pub fn add(a: i32, b: i32) -> i32 {"));
        assert!(text.contains("    a + b"));
        // Neighbouring functions must not leak.
        assert!(
            !text.contains("pub fn before()"),
            "leaked 'before' fn:\n{text}"
        );
        assert!(
            !text.contains("pub fn after()"),
            "leaked 'after' fn:\n{text}"
        );
    }

    // ---- miss reporting ---------------------------------------------

    #[test]
    fn missing_name_reported_in_footer_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "pub fn alpha() -> i32 { 1 }\n";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p], &["alpha", "ghost"]);
        assert!(
            !out.is_error,
            "soft miss alongside a hit must not flag is_error: {:?}",
            out.content
        );
        let text = body(&out);
        assert!(text.contains("pub fn alpha"));
        assert!(
            text.contains("not found in any provided file: ghost"),
            "missing miss footer in:\n{text}"
        );
    }

    #[test]
    fn all_names_missing_returns_miss_footer_with_no_hits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "pub fn alpha() {}\n";
        let p = write_file(dir.path(), "lib.rs", src);
        let out = run(&[&p], &["ghost1", "ghost2"]);
        // File parsed fine; no hits but file_successes > 0 → not is_error.
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("not found in any provided file"));
        assert!(text.contains("ghost1"));
        assert!(text.contains("ghost2"));
    }

    // ---- unsupported / read-fail handling ---------------------------

    #[test]
    fn unsupported_extension_partial_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = write_file(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let rs = write_file(dir.path(), "lib.rs", "pub fn alpha() -> i32 { 1 }\n");
        let out = run(&[&cfg, &rs], &["alpha"]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("no get_function support for .toml"));
        assert!(text.contains("pub fn alpha"));
    }

    #[test]
    fn all_unsupported_returns_is_error_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.toml", "x=1\n");
        let b = write_file(dir.path(), "b.toml", "y=2\n");
        let out = run(&[&a, &b], &["alpha"]);
        assert!(out.is_error, "all-failed must be flagged");
        let text = body(&out);
        assert!(text.contains("no get_function support for .toml"));
    }

    #[test]
    fn missing_file_partial_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = write_file(dir.path(), "lib.rs", "pub fn alpha() -> i32 { 1 }\n");
        let missing = dir.path().join("ghost.rs");
        let out = run(&[&real, &missing], &["alpha"]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("pub fn alpha"));
        assert!(text.contains("error: failed to read"));
    }

    // ---- arg validation ---------------------------------------------

    #[test]
    fn empty_paths_rejected() {
        let out = execute(
            serde_json::json!({"paths": [], "function_names": ["x"]}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("paths"));
    }

    #[test]
    fn empty_function_names_rejected() {
        let out = execute(
            serde_json::json!({"paths": ["a.rs"], "function_names": []}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("function_names"));
    }

    #[test]
    fn missing_arguments_rejected() {
        let out = execute(serde_json::json!({}), "/tmp", &CancelToken::new());
        assert!(out.is_error);
    }

    #[test]
    fn non_string_path_rejected() {
        let out = execute(
            serde_json::json!({"paths": [42], "function_names": ["x"]}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
    }

    #[test]
    fn non_string_name_rejected() {
        let out = execute(
            serde_json::json!({"paths": ["a.rs"], "function_names": [42]}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
    }

    #[test]
    fn empty_string_name_rejected() {
        let out = execute(
            serde_json::json!({"paths": ["a.rs"], "function_names": ["   "]}),
            "/tmp",
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(body(&out).contains("empty"));
    }

    // ---- TypeScript spot-check --------------------------------------

    #[test]
    fn typescript_class_method_resolves_qualified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = "\
export class Greeter {
  greet(name: string): string {
    const prefix = 'hello ';
    return prefix + name;
  }
}

export function plain(x: number): number {
  return x + 1;
}
";
        let p = write_file(dir.path(), "thing.ts", src);

        let out = run(&[&p], &["Greeter.greet"]);
        assert!(!out.is_error, "out: {:?}", out.content);
        let text = body(&out);
        assert!(text.contains("greet(name: string): string {"));
        assert!(text.contains("return prefix + name;"));
        assert!(!text.contains("plain"), "leaked plain():\n{text}");

        // Top-level function via dotless name.
        let out = run(&[&p], &["plain"]);
        assert!(!out.is_error);
        let text = body(&out);
        assert!(text.contains("export function plain"));
        assert!(text.contains("return x + 1;"));
    }
}
