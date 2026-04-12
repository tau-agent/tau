//! Project-specific instruction injection for task lifecycle phases.
//!
//! Loads custom instructions from up to three config tiers (operator >
//! project > global) and injects them into planning, refining, review, and
//! worker session prompts.
//!
//! ## Resolution order
//!
//! 1. **Operator** – `~/.config/tau/projects/{name}/instructions.toml`
//! 2. **Project** – `{project}/.tau/instructions.toml`
//! 3. **Global**  – `~/.config/tau/instructions.toml`
//!
//! Instructions from higher tiers are **prepended**: operator instructions
//! appear first, then project, then global.
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

use tau_agent_base::config_chain;

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

/// Load project instructions for the given `phase` from up to three
/// config tiers (operator > project > global).
///
/// Returns the combined text of all tiers' `[common].instructions` and
/// `[{phase}].instructions`, with operator instructions prepended, then
/// project, then global.  Returns `None` if no tier contains instructions
/// for the requested phase.
///
/// `phase` should be `"planning"`, `"refining"`, `"review"`, or
/// `"worker"`.  Any other value is accepted but will only match the
/// `[common]` section.
pub fn load_project_instructions(
    project: &str,
    project_name: Option<&str>,
    phase: &str,
) -> Option<String> {
    let configs: Vec<(_, InstructionsConfig)> = config_chain::load_all(
        project_name,
        Some(project),
        "instructions.toml",
        true, // instructions are not security-sensitive
    );

    let mut parts: Vec<&str> = Vec::new();

    for (_path, config) in &configs {
        let common = config.common.instructions.as_deref().map(str::trim);
        let phase_text = match phase {
            "planning" => config.planning.instructions.as_deref().map(str::trim),
            "refining" => config.refining.instructions.as_deref().map(str::trim),
            "review" => config.review.instructions.as_deref().map(str::trim),
            "worker" => config.worker.instructions.as_deref().map(str::trim),
            _ => None,
        };

        // Collect non-empty parts from this tier
        for text in [common, phase_text].into_iter().flatten() {
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }

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

    // Tests that modify env vars must be serialized.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct XdgGuard {
        prev_xdg: Option<String>,
        prev_home: Option<String>,
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
                None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
            }
            match &self.prev_home {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    /// Set XDG_CONFIG_HOME to an isolated temp directory so that real user
    /// config files don't interfere with tests.  Returns the temp dir and
    /// a guard that restores the original env vars on drop.
    fn isolate_config() -> (TempDir, XdgGuard) {
        let config_tmp = TempDir::new().unwrap();
        let guard = XdgGuard {
            prev_xdg: std::env::var("XDG_CONFIG_HOME").ok(),
            prev_home: std::env::var("HOME").ok(),
        };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_tmp.path()) };
        (config_tmp, guard)
    }

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
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let refining = load_project_instructions(project, None, "refining").unwrap();
        assert!(refining.contains("Follow coding style"));
        assert!(refining.contains("Check backward compat"));
        assert!(!refining.contains("Check error handling"));

        let review = load_project_instructions(project, None, "review").unwrap();
        assert!(review.contains("Follow coding style"));
        assert!(review.contains("Check error handling"));
        assert!(!review.contains("Check backward compat"));
    }

    #[test]
    fn only_common_section() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let refining = load_project_instructions(project, None, "refining").unwrap();
        assert!(refining.contains("Follow coding style"));
        assert!(refining.contains("Keep changes minimal"));

        let review = load_project_instructions(project, None, "review").unwrap();
        assert!(review.contains("Follow coding style"));
    }

    #[test]
    fn only_phase_section() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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
        assert!(load_project_instructions(project, None, "refining").is_none());

        // review does
        let review = load_project_instructions(project, None, "review").unwrap();
        assert!(review.contains("No unwrap() in non-test code"));
    }

    #[test]
    fn missing_file_returns_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = TempDir::new().unwrap();
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, None, "refining").is_none());
        assert!(load_project_instructions(project, None, "review").is_none());
    }

    #[test]
    fn malformed_toml_returns_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = setup("this is not valid [[ toml }{");
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, None, "review").is_none());
    }

    #[test]
    fn empty_instructions_returns_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = setup(
            r#"
[common]
instructions = ""

[review]
instructions = ""
"#,
        );
        let project = dir.path().to_str().unwrap();
        assert!(load_project_instructions(project, None, "review").is_none());
    }

    #[test]
    fn unknown_phase_returns_only_common() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = setup(
            r#"
[common]
instructions = "- Be careful"

[review]
instructions = "- Check errors"
"#,
        );
        let project = dir.path().to_str().unwrap();

        let result = load_project_instructions(project, None, "deploy").unwrap();
        assert!(result.contains("Be careful"));
        assert!(!result.contains("Check errors"));
    }

    #[test]
    fn worker_phase_loads_common_and_worker() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let worker = load_project_instructions(project, None, "worker").unwrap();
        assert!(worker.contains("Follow coding style"));
        assert!(worker.contains("Keep diffs small"));
        // Should not include the review-only section
        assert!(!worker.contains("Check error handling"));
    }

    #[test]
    fn worker_phase_falls_back_to_common() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let worker = load_project_instructions(project, None, "worker").unwrap();
        assert!(worker.contains("Follow coding style"));
    }

    #[test]
    fn worker_phase_only() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = setup(
            r#"
[worker]
instructions = """
- Prefer helper functions
"""
"#,
        );
        let project = dir.path().to_str().unwrap();

        let worker = load_project_instructions(project, None, "worker").unwrap();
        assert!(worker.contains("Prefer helper functions"));
        // refining/review have nothing, not even common
        assert!(load_project_instructions(project, None, "refining").is_none());
    }

    #[test]
    fn whitespace_trimming() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let review = load_project_instructions(project, None, "review").unwrap();
        // Should be trimmed — no leading/trailing newlines
        assert!(review.starts_with("- Rule A"));
        assert!(review.ends_with("- Rule C"));
    }

    // --- Three-tier tests ---

    #[test]
    fn three_tier_operator_prepended() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (config_tmp, _xdg) = isolate_config();

        // Set up global config
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("instructions.toml"),
            "[common]\ninstructions = \"- Global rule\"\n",
        )
        .unwrap();

        // Set up project config
        let project_tmp = TempDir::new().unwrap();
        let tau_dir = project_tmp.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(
            tau_dir.join("instructions.toml"),
            "[common]\ninstructions = \"- Project rule\"\n",
        )
        .unwrap();

        // Set up operator config
        let operator_dir = global_dir.join("projects").join("testproj");
        fs::create_dir_all(&operator_dir).unwrap();
        fs::write(
            operator_dir.join("instructions.toml"),
            "[common]\ninstructions = \"- Operator rule\"\n",
        )
        .unwrap();

        let project = project_tmp.path().to_str().unwrap();
        let result = load_project_instructions(project, Some("testproj"), "worker").unwrap();

        // Operator should come first, then project, then global
        let op_pos = result
            .find("Operator rule")
            .expect("operator rule should be present");
        let proj_pos = result
            .find("Project rule")
            .expect("project rule should be present");
        let glob_pos = result
            .find("Global rule")
            .expect("global rule should be present");
        assert!(
            op_pos < proj_pos,
            "operator ({}) should come before project ({})",
            op_pos,
            proj_pos
        );
        assert!(
            proj_pos < glob_pos,
            "project ({}) should come before global ({})",
            proj_pos,
            glob_pos
        );
    }

    #[test]
    fn three_tier_phase_specific() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (config_tmp, _xdg) = isolate_config();

        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("instructions.toml"),
            "[review]\ninstructions = \"- Global review\"\n",
        )
        .unwrap();

        let project_tmp = TempDir::new().unwrap();
        let tau_dir = project_tmp.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(
            tau_dir.join("instructions.toml"),
            "[worker]\ninstructions = \"- Project worker\"\n",
        )
        .unwrap();

        let project = project_tmp.path().to_str().unwrap();

        // Review phase: only global has review instructions
        let review = load_project_instructions(project, None, "review").unwrap();
        assert!(review.contains("Global review"));
        assert!(!review.contains("Project worker"));

        // Worker phase: only project has worker instructions
        let worker = load_project_instructions(project, None, "worker").unwrap();
        assert!(worker.contains("Project worker"));
        assert!(!worker.contains("Global review"));
    }

    #[test]
    fn three_tier_global_only_when_no_project_name() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (config_tmp, _xdg) = isolate_config();

        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("instructions.toml"),
            "[common]\ninstructions = \"- Global rule\"\n",
        )
        .unwrap();

        // Operator config exists but project_name is None — should not be loaded
        let operator_dir = global_dir.join("projects").join("testproj");
        fs::create_dir_all(&operator_dir).unwrap();
        fs::write(
            operator_dir.join("instructions.toml"),
            "[common]\ninstructions = \"- Operator rule\"\n",
        )
        .unwrap();

        let project_tmp = TempDir::new().unwrap();
        let project = project_tmp.path().to_str().unwrap();

        let result = load_project_instructions(project, None, "worker").unwrap();
        assert!(result.contains("Global rule"));
        assert!(!result.contains("Operator rule"));
    }
}
