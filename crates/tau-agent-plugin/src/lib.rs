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
    ]
}
