//! Project-specific model alias configuration.
//!
//! Loads `{project}/.tau/models.toml` and exposes its `[aliases]` map.
//!
//! ## File format
//!
//! ```toml
//! [aliases]
//! smart = "claude-opus-4-6"
//! fast = "claude-haiku-4"
//! # provider/id form to disambiguate when an id is registered under
//! # multiple providers:
//! cheap = "openai/gpt-4.1-mini"
//! ```
//!
//! Resolution semantics live in [`crate::model_resolve`].  This module is
//! only responsible for parsing the file from disk.

use serde::Deserialize;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Root structure of `.tau/models.toml`.
#[derive(Debug, Default, Deserialize)]
struct ModelsConfig {
    #[serde(default)]
    aliases: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load `{project}/.tau/models.toml` and return its alias map.
///
/// Returns an empty map when:
///   - the file does not exist
///   - the file is unreadable
///   - the TOML is malformed (a warning is printed to stderr)
///
/// This mirrors [`crate::tasks_config::load_project_instructions`]: failures
/// are non-fatal so that a broken file in one project doesn't break unrelated
/// commands in others.
pub fn load_project_aliases(project: &str) -> HashMap<String, String> {
    let path = std::path::Path::new(project)
        .join(".tau")
        .join("models.toml");

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    match toml::from_str::<ModelsConfig>(&content) {
        Ok(c) => c.aliases,
        Err(e) => {
            eprintln!("project_config: failed to parse {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a temp project dir with the given `.tau/models.toml` content.
    fn setup(toml_content: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(tau_dir.join("models.toml"), toml_content).unwrap();
        dir
    }

    #[test]
    fn missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_str().unwrap();
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn populated_aliases_are_loaded() {
        let dir = setup(
            r#"
[aliases]
smart = "claude-opus-4-6"
fast = "claude-haiku-4"
cheap = "openai/gpt-4.1-mini"
"#,
        );
        let project = dir.path().to_str().unwrap();
        let aliases = load_project_aliases(project);
        assert_eq!(aliases.len(), 3);
        assert_eq!(
            aliases.get("smart").map(String::as_str),
            Some("claude-opus-4-6")
        );
        assert_eq!(
            aliases.get("fast").map(String::as_str),
            Some("claude-haiku-4")
        );
        assert_eq!(
            aliases.get("cheap").map(String::as_str),
            Some("openai/gpt-4.1-mini")
        );
    }

    #[test]
    fn malformed_toml_returns_empty() {
        let dir = setup("this is not [[ valid toml }{");
        let project = dir.path().to_str().unwrap();
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn empty_aliases_section_returns_empty() {
        let dir = setup("[aliases]\n");
        let project = dir.path().to_str().unwrap();
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn no_aliases_section_returns_empty() {
        // File present but no [aliases] section.
        let dir = setup("# nothing here\n");
        let project = dir.path().to_str().unwrap();
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn nonexistent_project_dir_returns_empty() {
        // load_project_aliases on a path that doesn't exist must not panic.
        let aliases = load_project_aliases("/this/path/should/not/exist/12345");
        assert!(aliases.is_empty());
    }
}
