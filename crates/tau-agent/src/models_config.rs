//! Model alias configuration.
//!
//! Tau resolves model aliases from up to three files, all named `models.toml`:
//!
//! | Scope    | File                                       |
//! |----------|--------------------------------------------|
//! | Operator | `~/.config/tau/projects/{name}/models.toml` |
//! | Project  | `{project}/.tau/models.toml`               |
//! | Global   | `~/.config/tau/models.toml`                |
//!
//! Resolution order (implemented in [`crate::model_resolve`]):
//!
//!   1. operator alias map (from `~/.config/tau/projects/{name}/models.toml`)
//!   2. project alias map (from `{cwd}/.tau/models.toml`)
//!   3. global alias map (from `~/.config/tau/models.toml`)
//!   4. literal model id
//!
//! Only one alias hop is performed, so alias targets must be model ids
//! (optionally `provider/model-id`), never other aliases.
//!
//! ## File format
//!
//! All files share the same shape:
//!
//! ```toml
//! [aliases]
//! smart = "claude-opus-4-6"
//! fast  = "claude-haiku-4"
//! # provider/id form to disambiguate when an id is registered under
//! # multiple providers:
//! cheap = "openai/gpt-4.1-mini"
//! ```
//!
//! ## Migration from `providers.toml`
//!
//! Earlier versions of tau kept the global `[aliases]` section in
//! `~/.config/tau/providers.toml` alongside provider definitions.  That
//! location is now deprecated.  [`load_global_aliases`] still reads those
//! legacy entries as a fallback but prints a one-shot warning to stderr
//! the first time it does so, asking the user to move them to
//! `~/.config/tau/models.toml`.
//!
//! This module is only responsible for parsing the files from disk.  The
//! actual alias lookup logic lives in [`crate::model_resolve`].

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Root structure of `models.toml` (global or per-project).
#[derive(Debug, Default, Deserialize)]
struct ModelsConfig {
    #[serde(default)]
    aliases: HashMap<String, String>,
}

/// Minimal shape for parsing only the legacy `[aliases]` section out of
/// `providers.toml` without caring about the rest of that file's schema.
#[derive(Debug, Default, Deserialize)]
struct LegacyProvidersAliases {
    #[serde(default)]
    aliases: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Path to the global `models.toml` file
/// (`$XDG_CONFIG_HOME/tau/models.toml` or `~/.config/tau/models.toml`).
pub fn global_models_path() -> PathBuf {
    crate::paths::config_dir().join("models.toml")
}

/// Path to the legacy `providers.toml` file (kept here only so the
/// migration warning uses the same resolution as [`crate::config`]).
fn legacy_providers_path() -> PathBuf {
    crate::paths::config_dir().join("providers.toml")
}

/// Load the global alias map from `~/.config/tau/models.toml`.
///
/// Falls back to reading `[aliases]` from `~/.config/tau/providers.toml`
/// when the new file is missing or empty — this is the legacy location
/// and is deprecated.  A one-line warning is printed to stderr in that
/// case so users know to migrate.
///
/// Returns an empty map when:
///   - neither file exists
///   - a file is unreadable
///   - the TOML is malformed (a warning is printed to stderr)
///
/// Failures are non-fatal, mirroring [`load_project_aliases`] — a broken
/// config file should never take the whole server down.
pub fn load_global_aliases() -> HashMap<String, String> {
    let new_path = global_models_path();
    let new_aliases = read_models_toml(&new_path);

    // Check the legacy location regardless of whether the new file
    // produced aliases, so we can warn on the double-definition case too.
    let legacy_path = legacy_providers_path();
    let legacy_aliases = read_legacy_providers_aliases(&legacy_path);

    match (new_aliases.is_empty(), legacy_aliases.is_empty()) {
        (true, true) => HashMap::new(),
        (false, true) => new_aliases,
        (true, false) => {
            eprintln!(
                "warning: [aliases] in {} is deprecated; move it to {}",
                legacy_path.display(),
                new_path.display(),
            );
            legacy_aliases
        }
        (false, false) => {
            // Both present: new file wins, but still warn about the
            // stale legacy entries so the user cleans them up.
            eprintln!(
                "warning: [aliases] in {} is deprecated and shadowed by {}; \
                 remove the legacy section to silence this warning",
                legacy_path.display(),
                new_path.display(),
            );
            new_aliases
        }
    }
}

/// Load `{project}/.tau/models.toml` and return its alias map.
///
/// Returns an empty map when:
///   - the file does not exist
///   - the file is unreadable
///   - the TOML is malformed (a warning is printed to stderr)
///
/// This mirrors `tau_agent_plugin_tasks::tasks_config::load_project_instructions`:
/// failures are non-fatal so that a broken file in one project doesn't
/// break unrelated commands in others.
pub fn load_project_aliases(project: &str) -> HashMap<String, String> {
    let path = Path::new(project).join(".tau").join("models.toml");
    read_models_toml(&path)
}

/// Load model aliases from the operator config directory.
///
/// Reads `~/.config/tau/projects/{project_name}/models.toml` and returns
/// its `[aliases]` map.  Returns an empty map if the file doesn't exist,
/// is unreadable, or is malformed (a warning is printed to stderr).
pub fn load_operator_aliases(project_name: &str) -> HashMap<String, String> {
    let path = crate::paths::project_config_dir(project_name).join("models.toml");
    read_models_toml(&path)
}

/// Merge alias maps in priority order (operator > project).
///
/// Operator entries override project entries.  The merged result can be
/// passed to `resolve_model` as the `project_aliases` parameter — this
/// avoids changing the resolver's signature while adding operator-tier
/// support.
pub fn merge_alias_maps(
    operator: HashMap<String, String>,
    project: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = project;
    merged.extend(operator); // operator entries override
    merged
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a `models.toml`-shaped file and return its `[aliases]` map.
fn read_models_toml(path: &Path) -> HashMap<String, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    match toml::from_str::<ModelsConfig>(&content) {
        Ok(c) => c.aliases,
        Err(e) => {
            eprintln!("models_config: failed to parse {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

/// Read the legacy `[aliases]` section out of `providers.toml`.  Returns
/// an empty map if the file is missing, unreadable, malformed, or has no
/// `[aliases]` key.
fn read_legacy_providers_aliases(path: &Path) -> HashMap<String, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    match toml::from_str::<LegacyProvidersAliases>(&content) {
        Ok(c) => c.aliases,
        Err(_) => {
            // We deliberately don't print a parse warning here: the
            // providers.toml parser in `crate::config` will report that
            // error with a proper context line.  If we warned as well
            // the user would see the same thing twice.
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

    // -- project-level ------------------------------------------------------

    /// Create a temp project dir with the given `.tau/models.toml` content.
    fn setup_project(toml_content: &str) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let tau_dir = dir.path().join(".tau");
        fs::create_dir_all(&tau_dir).expect("mkdir .tau");
        fs::write(tau_dir.join("models.toml"), toml_content).expect("write models.toml");
        dir
    }

    #[test]
    fn project_missing_file_returns_empty() {
        let dir = TempDir::new().expect("tempdir");
        let project = dir.path().to_str().expect("utf-8 path");
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn project_populated_aliases_are_loaded() {
        let dir = setup_project(
            r#"
[aliases]
smart = "claude-opus-4-6"
fast = "claude-haiku-4"
cheap = "openai/gpt-4.1-mini"
"#,
        );
        let project = dir.path().to_str().expect("utf-8 path");
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
    fn project_malformed_toml_returns_empty() {
        let dir = setup_project("this is not [[ valid toml }{");
        let project = dir.path().to_str().expect("utf-8 path");
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn project_empty_aliases_section_returns_empty() {
        let dir = setup_project("[aliases]\n");
        let project = dir.path().to_str().expect("utf-8 path");
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn project_no_aliases_section_returns_empty() {
        let dir = setup_project("# nothing here\n");
        let project = dir.path().to_str().expect("utf-8 path");
        let aliases = load_project_aliases(project);
        assert!(aliases.is_empty());
    }

    #[test]
    fn project_nonexistent_dir_returns_empty() {
        let aliases = load_project_aliases("/this/path/should/not/exist/12345");
        assert!(aliases.is_empty());
    }

    // -- global -------------------------------------------------------------

    /// Build a temp dir that doubles as `XDG_CONFIG_HOME/tau` and return
    /// a guard that points `XDG_CONFIG_HOME` at it.  The guard restores
    /// the previous value when dropped.
    struct XdgGuard {
        _dir: TempDir,
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

    /// Prepare an isolated `$XDG_CONFIG_HOME` pointing at a fresh temp
    /// directory, and optionally populate it with a global `models.toml`
    /// and/or a legacy `providers.toml`.
    fn setup_global(models_toml: Option<&str>, providers_toml: Option<&str>) -> XdgGuard {
        let dir = TempDir::new().expect("tempdir");
        let tau_dir = dir.path().join("tau");
        fs::create_dir_all(&tau_dir).expect("mkdir tau");
        if let Some(contents) = models_toml {
            fs::write(tau_dir.join("models.toml"), contents).expect("write models.toml");
        }
        if let Some(contents) = providers_toml {
            fs::write(tau_dir.join("providers.toml"), contents).expect("write providers.toml");
        }
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        XdgGuard {
            _dir: dir,
            prev_xdg,
            prev_home,
        }
    }

    // NOTE: these tests mutate process-global env vars and therefore
    // must not run in parallel with each other.  Cargo runs tests within
    // a single file in parallel by default, so we serialize with a
    // static mutex.  Using `std::sync::Mutex` (not parking_lot) keeps
    // the dep list small.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn global_missing_both_files_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(None, None);
        let aliases = load_global_aliases();
        assert!(aliases.is_empty());
    }

    #[test]
    fn global_loads_from_models_toml() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(
            Some(
                r#"
[aliases]
smart = "claude-opus-4-6"
fast = "gpt-4.1-mini"
"#,
            ),
            None,
        );
        let aliases = load_global_aliases();
        assert_eq!(aliases.len(), 2);
        assert_eq!(
            aliases.get("smart").map(String::as_str),
            Some("claude-opus-4-6")
        );
        assert_eq!(
            aliases.get("fast").map(String::as_str),
            Some("gpt-4.1-mini")
        );
    }

    #[test]
    fn global_falls_back_to_legacy_providers_toml() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(
            None,
            Some(
                r#"
[providers.openai]
api = "openai"
base_url = "https://api.openai.com/v1"

[aliases]
light = "claude-sonnet-4-6"
"#,
            ),
        );
        let aliases = load_global_aliases();
        assert_eq!(aliases.len(), 1);
        assert_eq!(
            aliases.get("light").map(String::as_str),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn global_new_file_shadows_legacy() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(
            Some(
                r#"
[aliases]
smart = "new-model"
"#,
            ),
            Some(
                r#"
[providers.openai]
api = "openai"
base_url = "https://api.openai.com/v1"

[aliases]
smart = "legacy-model"
"#,
            ),
        );
        let aliases = load_global_aliases();
        assert_eq!(aliases.len(), 1);
        assert_eq!(
            aliases.get("smart").map(String::as_str),
            Some("new-model"),
            "new file must win when both define the same alias"
        );
    }

    #[test]
    fn global_malformed_models_toml_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(Some("this is not [[ valid toml }{"), None);
        let aliases = load_global_aliases();
        assert!(aliases.is_empty());
    }

    #[test]
    fn global_providers_toml_without_aliases_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(
            None,
            Some(
                r#"
[providers.openai]
api = "openai"
base_url = "https://api.openai.com/v1"
"#,
            ),
        );
        let aliases = load_global_aliases();
        assert!(aliases.is_empty());
    }

    // -- operator aliases ---------------------------------------------------

    #[test]
    fn operator_loads_from_operator_dir() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(None, None);

        // Create operator config for project "myproj"
        let operator_dir = crate::paths::project_config_dir("myproj");
        fs::create_dir_all(&operator_dir).expect("mkdir operator dir");
        fs::write(
            operator_dir.join("models.toml"),
            r#"
[aliases]
smart = "operator-model"
"#,
        )
        .expect("write operator models.toml");

        let aliases = load_operator_aliases("myproj");
        assert_eq!(aliases.len(), 1);
        assert_eq!(
            aliases.get("smart").map(String::as_str),
            Some("operator-model")
        );
    }

    #[test]
    fn operator_missing_dir_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = setup_global(None, None);

        let aliases = load_operator_aliases("nonexistent-project");
        assert!(aliases.is_empty());
    }

    // -- merge_alias_maps ---------------------------------------------------

    #[test]
    fn merge_operator_overrides_project() {
        let mut operator = HashMap::new();
        operator.insert("smart".into(), "operator-model".into());
        operator.insert("op-only".into(), "op-value".into());

        let mut project = HashMap::new();
        project.insert("smart".into(), "project-model".into());
        project.insert("proj-only".into(), "proj-value".into());

        let merged = merge_alias_maps(operator, project);
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged.get("smart").map(String::as_str),
            Some("operator-model"),
            "operator should override project"
        );
        assert_eq!(merged.get("op-only").map(String::as_str), Some("op-value"));
        assert_eq!(
            merged.get("proj-only").map(String::as_str),
            Some("proj-value")
        );
    }

    #[test]
    fn merge_empty_maps() {
        let merged = merge_alias_maps(HashMap::new(), HashMap::new());
        assert!(merged.is_empty());
    }
}
