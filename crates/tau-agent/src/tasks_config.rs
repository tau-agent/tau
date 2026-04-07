//! Project-specific instruction injection for task lifecycle phases.
//!
//! Loads custom instructions from `{project_root}/.tau/instructions.toml`
//! and injects them into planning, refining, review, and worker session
//! prompts.
//!
//! ## Config format
//!
//! ```toml
//! [common]
//! instructions = """
//! - Follow the project's coding style
//! """
//!
//! [refining]
//! instructions = """
//! - Verify backward compatibility
//! """
//!
//! [review]
//! instructions = """
//! - Check for proper error handling
//! """
//!
//! [worker]
//! instructions = """
//! - Keep diffs small and focused
//! """
//! ```

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Root structure of `.tau/instructions.toml`.
#[derive(Debug, Default, Deserialize)]
struct InstructionsConfig {
    #[serde(default)]
    common: Section,
    #[serde(default)]
    planning: Section,
    #[serde(default)]
    refining: Section,
    #[serde(default)]
    review: Section,
    #[serde(default)]
    worker: Section,
}

/// A single section containing optional instructions text.
#[derive(Debug, Default, Deserialize)]
struct Section {
    #[serde(default)]
    instructions: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load project instructions for the given `phase` from
/// `{project}/.tau/instructions.toml`.
///
/// Returns the combined text of `[common].instructions` and
/// `[{phase}].instructions`, separated by a blank line.  Returns `None` if
/// the file doesn't exist or has no instructions for the requested phase.
///
/// `phase` should be `"planning"`, `"refining"`, `"review"`, or
/// `"worker"`.  Any other value is accepted but will only match the
/// `[common]` section.
pub fn load_project_instructions(project: &str, phase: &str) -> Option<String> {
    let path = std::path::Path::new(project)
        .join(".tau")
        .join("instructions.toml");

    let content = std::fs::read_to_string(&path).ok()?;

    let config: InstructionsConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tasks_config: failed to parse {}: {}", path.display(), e);
            return None;
        }
    };

    let common = config.common.instructions.as_deref().map(str::trim);
    let phase_section = match phase {
        "planning" => &config.planning,
        "refining" => &config.refining,
        "review" => &config.review,
        "worker" => &config.worker,
        _ => &Section::default(),
    };
    let phase_text = phase_section.instructions.as_deref().map(str::trim);

    // Combine non-empty parts
    let parts: Vec<&str> = [common, phase_text]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
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

    /// Helper: create a temp project dir with the given instructions.toml content.
    fn setup(toml_content: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(tau_dir.join("instructions.toml"), toml_content).unwrap();
        dir
    }

    #[test]
    fn both_sections_present() {
        let dir = setup(
            r#"
[common]
instructions = """
- Follow coding style
"""

[refining]
instructions = """
- Check backward compat
"""

[review]
instructions = """
- Check error handling
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let refining = load_project_instructions(project, "refining").unwrap();
        assert!(refining.contains("Follow coding style"));
        assert!(refining.contains("Check backward compat"));
        assert!(!refining.contains("Check error handling"));

        let review = load_project_instructions(project, "review").unwrap();
        assert!(review.contains("Follow coding style"));
        assert!(review.contains("Check error handling"));
        assert!(!review.contains("Check backward compat"));
    }

    #[test]
    fn only_common_section() {
        let dir = setup(
            r#"
[common]
instructions = """
- Follow coding style
- Keep changes minimal
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let refining = load_project_instructions(project, "refining").unwrap();
        assert!(refining.contains("Follow coding style"));
        assert!(refining.contains("Keep changes minimal"));

        let review = load_project_instructions(project, "review").unwrap();
        assert!(review.contains("Follow coding style"));
    }

    #[test]
    fn only_phase_section() {
        let dir = setup(
            r#"
[review]
instructions = """
- No unwrap() in non-test code
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        // refining has no instructions
        assert!(load_project_instructions(project, "refining").is_none());

        // review does
        let review = load_project_instructions(project, "review").unwrap();
        assert!(review.contains("No unwrap() in non-test code"));
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, "refining").is_none());
        assert!(load_project_instructions(project, "review").is_none());
    }

    #[test]
    fn malformed_toml_returns_none() {
        let dir = setup("this is not valid [[ toml }{");
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, "review").is_none());
    }

    #[test]
    fn empty_instructions_returns_none() {
        let dir = setup(
            r#"
[common]
instructions = ""

[review]
instructions = ""
"#,
        );
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, "review").is_none());
    }

    #[test]
    fn unknown_phase_returns_only_common() {
        let dir = setup(
            r#"
[common]
instructions = "- Be careful"

[review]
instructions = "- Check errors"
"#,
        );
        let project = dir.path().to_str().unwrap();

        let result = load_project_instructions(project, "deploy").unwrap();
        assert!(result.contains("Be careful"));
        assert!(!result.contains("Check errors"));
    }

    #[test]
    fn worker_phase_loads_common_and_worker() {
        let dir = setup(
            r#"
[common]
instructions = """
- Follow coding style
"""

[worker]
instructions = """
- Keep diffs small
"""

[review]
instructions = """
- Check error handling
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let worker = load_project_instructions(project, "worker").unwrap();
        assert!(worker.contains("Follow coding style"));
        assert!(worker.contains("Keep diffs small"));
        // Should not include the review-only section
        assert!(!worker.contains("Check error handling"));
    }

    #[test]
    fn worker_phase_falls_back_to_common() {
        // No [worker] section — worker should still receive [common].
        let dir = setup(
            r#"
[common]
instructions = """
- Follow coding style
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let worker = load_project_instructions(project, "worker").unwrap();
        assert!(worker.contains("Follow coding style"));
    }

    #[test]
    fn worker_phase_only() {
        let dir = setup(
            r#"
[worker]
instructions = """
- Prefer helper functions
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let worker = load_project_instructions(project, "worker").unwrap();
        assert!(worker.contains("Prefer helper functions"));
        // refining/review have nothing, not even common
        assert!(load_project_instructions(project, "refining").is_none());
    }

    #[test]
    fn whitespace_trimming() {
        let dir = setup(
            r#"
[common]
instructions = """

  - Rule A
  - Rule B

"""

[review]
instructions = """

  - Rule C

"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let review = load_project_instructions(project, "review").unwrap();
        // Should be trimmed — no leading/trailing newlines
        assert!(review.starts_with("- Rule A"));
        assert!(review.ends_with("- Rule C"));
    }
}
