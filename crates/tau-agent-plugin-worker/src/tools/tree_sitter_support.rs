//! Shared tree-sitter plumbing for the source-code analysis tools.
//!
//! Both `get_file_skeleton` (`tools/skeleton.rs`) and `get_function`
//! (`tools/get_function.rs`) rely on the same per-language grammar +
//! tag-style query infrastructure. Centralising it here avoids duplicate
//! parsers, duplicate query strings, and per-call recompilation of the
//! same `Query`.
//!
//! The queries are direct ports of Dirac's
//! `src/services/tree-sitter/queries/{rust,python,typescript,
//! javascript}.ts`, restricted to the captures we care about (we drop the
//! `@name.reference.*` clauses since neither tool needs cross-references
//! for v1). Crucially we keep **both** the `@name.definition.*` captures
//! (used by skeleton to render header lines) **and** the wrapping
//! `@definition.*` captures (used by `get_function` to walk the parent
//! chain and produce dotted full names like `Foo.bar`).
//!
//! ### Capture-name conventions
//!
//! Every match emits at least one of:
//! - `name.definition.<kind>` — the identifier node naming the definition.
//!   Its row is the *signature line*; its byte range is the bare name.
//! - `definition.<kind>` — the whole definition node (function, class,
//!   impl, …). Its byte range covers the body; ancestor walks key off it.
//! - `doc` (Rust + Python only) — leading doc comments, present so the
//!   capture exists but currently unused by either tool.
//!
//! ### Caching
//!
//! `Query` compilation is one-shot per language per process. We use
//! `OnceLock<&'static Query>` per variant (no async, no re-entrancy
//! concerns). Parsers are kept in a thread-local cell — `tree-sitter`'s
//! `Parser` is `!Sync` and reusing one across parses is the documented
//! happy path.

use std::cell::RefCell;
use std::sync::OnceLock;
use tree_sitter::{Language, Parser, Query, Tree};

/// Languages with first-class skeleton / function-extraction support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
}

impl Lang {
    /// Best-effort dispatch from a file extension. Returns `None` for any
    /// extension we don't have a grammar for; callers translate that into
    /// a per-file "no support for .<ext>; use `read` instead" message.
    pub(crate) fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    /// The `tree-sitter` `Language` handle for this grammar.
    pub(crate) fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    /// The full tag-query source for this language. Includes both
    /// `name.definition.*` and `definition.*` captures so both consumers
    /// (skeleton + get_function) can pick the captures they care about.
    pub(crate) fn tag_query(self) -> &'static str {
        match self {
            Self::Rust => RUST_QUERY,
            Self::Python => PYTHON_QUERY,
            Self::JavaScript => JAVASCRIPT_QUERY,
            Self::TypeScript | Self::Tsx => TYPESCRIPT_QUERY,
        }
    }
}

// ----- query strings ---------------------------------------------------

/// Rust tag query.
///
/// The `impl_item` clause was extended (vs Dirac's stripped version) to
/// also capture the implemented type as `@name.definition.class`. That
/// gives `get_function` something to dot-prefix methods with — without it
/// methods on `impl Foo` would resolve to a bare name with no `Foo.`
/// prefix. Skeleton happily picks up the new capture too: the
/// `name.definition.class` node sits on the same line as `impl Foo {`, so
/// the rendered header line is unchanged.
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

;; Impl blocks — capture the implemented type as the "name" so the
;; skeleton header line still appears AND get_function can prepend `Foo.`
;; to method names. Covers `impl Foo`, `impl Foo<T>`, `impl path::Foo`,
;; `impl Foo for Bar` (in which case `type:` is `Bar`, the type being
;; implemented for, which is what we want for method dispatch).
(impl_item
  type: [
    (type_identifier)
    (generic_type)
    (scoped_type_identifier)
  ] @name.definition.class) @definition.class

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

// ----- compiled-query cache -------------------------------------------

static RUST_Q: OnceLock<Query> = OnceLock::new();
static PYTHON_Q: OnceLock<Query> = OnceLock::new();
static JAVASCRIPT_Q: OnceLock<Query> = OnceLock::new();
static TYPESCRIPT_Q: OnceLock<Query> = OnceLock::new();
static TSX_Q: OnceLock<Query> = OnceLock::new();

fn compile_query(lang: Lang) -> Query {
    Query::new(&lang.ts_language(), lang.tag_query())
        .expect("bundled tag query for language must compile (this is a static asset)")
}

/// Borrow the compiled `Query` for `lang`. Compiled once per process.
pub(crate) fn query_for(lang: Lang) -> &'static Query {
    let cell = match lang {
        Lang::Rust => &RUST_Q,
        Lang::Python => &PYTHON_Q,
        Lang::JavaScript => &JAVASCRIPT_Q,
        Lang::TypeScript => &TYPESCRIPT_Q,
        Lang::Tsx => &TSX_Q,
    };
    cell.get_or_init(|| compile_query(lang))
}

// ----- thread-local parser pool ---------------------------------------

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

/// Parse `source` with `lang`'s grammar, returning the resulting tree.
///
/// Returns an error message string if `set_language` fails (a bundled
/// grammar mismatch — should not happen in practice) or if tree-sitter
/// returns `None` (parser cancelled or out of memory). Per-tool callers
/// surface this as a per-file error and continue with the next path.
pub(crate) fn parse(lang: Lang, source: &str) -> Result<Tree, String> {
    PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        parser
            .set_language(&lang.ts_language())
            .map_err(|e| format!("set_language: {e}"))?;
        parser
            .parse(source, None)
            .ok_or_else(|| "tree-sitter parse failed".to_string())
    })
}
