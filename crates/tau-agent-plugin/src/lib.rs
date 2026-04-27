//! Plugin SDK for the tau agent.
//!
//! This crate provides everything a plugin author needs in one place:
//! - The `ToolExecutor` trait (the tool execution abstraction)
//! - The `tunnel` module for plugin ↔ server communication
//! - Re-exports of all plugin-relevant types from `tau-agent-base`

pub mod executor;
pub mod tunnel;

// Re-export the ToolExecutor trait at crate root for convenience
pub use executor::ToolExecutor;

// Re-export plugin-relevant types from tau-agent-base
pub use tau_agent_base::paths::data_dir;
pub use tau_agent_base::plugin_protocol::*;
pub use tau_agent_base::protocol::{Request, Response, SessionInfo};
pub use tau_agent_base::tool_prompt::ToolPrompt;
pub use tau_agent_base::types::*;
pub use tau_agent_base::{Error, Result, read_json_line, write_json_line};

/// Built-in tool prompts for the default tools (bash, read, edit, write).
///
/// This is the canonical single source for these prompts. Both the engine's
/// `system_prompt::build_default` and the worker crate delegate here.
pub fn default_tool_prompts() -> Vec<ToolPrompt> {
    vec![
        ToolPrompt {
            name: "bash".into(),
            snippet: "Execute bash commands (ls, grep, find, etc.)".into(),
            guidelines: vec!["Use bash for file operations like ls, rg, find".into()],
        },
        ToolPrompt {
            name: "read".into(),
            snippet: "Read file contents".into(),
            guidelines: vec![
                "Use read to examine files instead of cat or sed.".into(),
                "Each line in the output is prefixed with `<hash>§` (or `<hash>.<n>§` for duplicate lines) — these are stable per-line anchors you can pass to the edit tool's anchor shape.".into(),
            ],
        },
        ToolPrompt {
            name: "edit".into(),
            snippet: "Make precise file edits with exact text replacement or by line-anchor, including multiple disjoint edits in one call across one or more files".into(),
            guidelines: vec![
                "Use edit for precise changes (old text must match exactly)".into(),
                "When changing multiple separate locations in one file, use one edit call with edits[] instead of multiple edit calls".into(),
                "For refactors that touch several files, batch them into one edit call using `files: [{path, edits: [...]}, ...]` instead of issuing one edit call per file.".into(),
                "Each edits[].old_text is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".into(),
                "Keep edits[].old_text as small as possible while still being unique in the file. Do not pad with large unchanged regions.".into(),
                "Anchor shape: `{edit_type: \"replace\"|\"insert_before\"|\"insert_after\", anchor, end_anchor?, text}`. `anchor` / `end_anchor` come from the read tool's `<hash>§` prefixes (the bare token or the full hashed line both work). `replace` is inclusive on both ends; omit `end_anchor` for a single-line replace.".into(),
                "Prefer the anchor shape over `old_text` when whitespace is fiddly or the text appears more than once — anchors sidestep both problems.".into(),
                "Anchors are validated against the file's current content. If you get an `anchor not found` error, re-read the file to get fresh anchors and try again.".into(),
            ],
        },
        ToolPrompt {
            name: "write".into(),
            snippet: "Create or overwrite files".into(),
            guidelines: vec!["Use write only for new files or complete rewrites.".into()],
        },
        ToolPrompt {
            name: "get_file_skeleton".into(),
            snippet: "Quickly outline source files (classes / functions / methods, no bodies) using tree-sitter".into(),
            guidelines: vec![
                "Prefer get_file_skeleton over reading whole files when surveying a codebase — pass several paths in one call to skim 10 files for ~5% the tokens of reading them, then use `read` for the parts you need to see in full.".into(),
                "Supports Rust, Python, JavaScript, TypeScript, and TSX. Other extensions return a per-file error suggesting `read` instead — partial success is fine, only an all-fail call is flagged as an error.".into(),
            ],
        },
        ToolPrompt {
            name: "get_function".into(),
            snippet: "Extract complete bodies of named functions/methods from one or more files (tree-sitter)".into(),
            guidelines: vec![
                "Use get_function to drill into 1-2 specific functions in a large file instead of `read`-ing the whole thing.".into(),
                "Function names support dot-paths for methods (`Foo.bar`, `ClassName.methodName`); `::` is also accepted for Rust. Bare names (`bar`) match any definition whose qualified name ends in `.bar` — multiple matches are returned together.".into(),
                "Pairs with get_file_skeleton: skim the skeleton first, then pull bodies you actually need.".into(),
            ],
        },
        ToolPrompt {
            name: "diagnostics_scan".into(),
            snippet: "Run lint/syntax diagnostics on specific files and get structured per-file feedback (built-in: Rust via cargo check; configurable via .tau/diagnostics.toml)".into(),
            guidelines: vec![
                "Use diagnostics_scan after edits to verify the file compiles/lints cleanly, instead of running full-project `cargo check` via bash.".into(),
                "Pass only the files you actually changed; the tool resolves project context automatically.".into(),
                "Output is structured JSON ({summary, diagnostics[], skipped[]}). is_error is false even when diagnostics are present — read the JSON to count errors.".into(),
            ],
        },
    ]
}
