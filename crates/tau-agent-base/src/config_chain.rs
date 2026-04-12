//! Three-tier config resolution: operator > project > global.
//!
//! Resolution order (highest priority first):
//!
//! 1. **Operator tier** – `~/.config/tau/projects/{name}/{filename}`
//!    (only when a project name is known)
//! 2. **Project tier** – `{project_path}/.tau/{filename}`
//!    (only when allowed — security-sensitive files like `sandbox.toml`
//!    and `plugins.toml` skip this tier)
//! 3. **Global tier** – `~/.config/tau/{filename}`
//!
//! Callers choose how to merge the results:
//!
//! - `instructions.toml` / `checklist.toml`: concatenate (operator first)
//! - `models.toml`: first-match-wins per key (operator > project > global)
//! - `sandbox.toml` / `plugins.toml`: use `load_first` with
//!   `allow_project_tier = false`

use std::path::PathBuf;

use crate::paths;

/// Build the ordered list of candidate paths for a config file.
///
/// Paths are returned in priority order (highest first).  Not all paths
/// will exist on disk — callers filter by existence.
pub fn config_paths(
    project_name: Option<&str>,
    project_path: Option<&str>,
    filename: &str,
    allow_project_tier: bool,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(3);

    // 1. Operator tier
    if let Some(name) = project_name {
        paths.push(paths::project_config_dir(name).join(filename));
    }

    // 2. Project tier (skipped for security-sensitive configs)
    if allow_project_tier {
        if let Some(project) = project_path {
            paths.push(PathBuf::from(project).join(".tau").join(filename));
        }
    }

    // 3. Global tier
    paths.push(paths::config_dir().join(filename));

    paths
}

/// Load and deserialize a TOML config file from the highest-priority tier
/// where it exists.
///
/// Returns `None` when no file is found at any tier, or when all found
/// files fail to parse (warnings are printed to stderr).
pub fn load_first<T: serde::de::DeserializeOwned>(
    project_name: Option<&str>,
    project_path: Option<&str>,
    filename: &str,
    allow_project_tier: bool,
) -> Option<T> {
    for path in config_paths(project_name, project_path, filename, allow_project_tier) {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match toml::from_str::<T>(&content) {
            Ok(val) => return Some(val),
            Err(e) => {
                eprintln!("config_chain: failed to parse {}: {}", path.display(), e);
            }
        }
    }
    None
}

/// Load and deserialize TOML config files from all tiers where they exist.
///
/// Returns a vec of `(path, parsed_value)` pairs in priority order
/// (highest first).  Files that don't exist or fail to parse are silently
/// skipped (malformed files print a warning to stderr).
///
/// Callers merge the results according to their own semantics.
pub fn load_all<T: serde::de::DeserializeOwned>(
    project_name: Option<&str>,
    project_path: Option<&str>,
    filename: &str,
    allow_project_tier: bool,
) -> Vec<(PathBuf, T)> {
    let mut results = Vec::new();
    for path in config_paths(project_name, project_path, filename, allow_project_tier) {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match toml::from_str::<T>(&content) {
            Ok(val) => results.push((path, val)),
            Err(e) => {
                eprintln!("config_chain: failed to parse {}: {}", path.display(), e);
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Use the crate-wide env mutex to avoid races with other modules
    // (project.rs etc.) that also mutate XDG_CONFIG_HOME.

    /// Guard that overrides XDG_CONFIG_HOME and restores it on drop.
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

    fn set_xdg(dir: &std::path::Path) -> XdgGuard {
        let guard = XdgGuard {
            prev_xdg: std::env::var("XDG_CONFIG_HOME").ok(),
            prev_home: std::env::var("HOME").ok(),
        };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir) };
        guard
    }

    // --- config_paths tests ---

    #[test]
    fn config_paths_all_tiers() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());

        let paths = config_paths(
            Some("myproj"),
            Some("/home/user/project"),
            "instructions.toml",
            true,
        );

        assert_eq!(paths.len(), 3);
        assert!(
            paths[0]
                .to_str()
                .unwrap()
                .contains("projects/myproj/instructions.toml")
        );
        assert!(
            paths[1]
                .to_str()
                .unwrap()
                .contains("/home/user/project/.tau/instructions.toml")
        );
        assert!(paths[2].to_str().unwrap().contains("tau/instructions.toml"));
        // Global should NOT contain "projects/"
        assert!(!paths[2].to_str().unwrap().contains("projects/"));
    }

    #[test]
    fn config_paths_skip_project_tier() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());

        let paths = config_paths(
            Some("myproj"),
            Some("/home/user/project"),
            "sandbox.toml",
            false, // security-sensitive: skip project tier
        );

        assert_eq!(paths.len(), 2);
        assert!(
            paths[0]
                .to_str()
                .unwrap()
                .contains("projects/myproj/sandbox.toml")
        );
        assert!(paths[1].to_str().unwrap().contains("tau/sandbox.toml"));
        // No project tier path
        assert!(
            paths
                .iter()
                .all(|p| !p.to_str().unwrap().contains(".tau/sandbox.toml"))
        );
    }

    #[test]
    fn config_paths_no_project_name() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());

        let paths = config_paths(None, Some("/home/user/project"), "instructions.toml", true);

        assert_eq!(paths.len(), 2);
        // No operator tier
        assert!(!paths[0].to_str().unwrap().contains("projects/"));
        assert!(
            paths[0]
                .to_str()
                .unwrap()
                .contains(".tau/instructions.toml")
        );
        assert!(paths[1].to_str().unwrap().contains("tau/instructions.toml"));
    }

    #[test]
    fn config_paths_no_project_at_all() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());

        let paths = config_paths(None, None, "models.toml", true);

        assert_eq!(paths.len(), 1);
        assert!(paths[0].to_str().unwrap().contains("tau/models.toml"));
    }

    // --- load_all tests ---

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SimpleConfig {
        #[serde(default)]
        value: Option<String>,
    }

    #[test]
    fn load_all_returns_all_tiers() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Set up global config
        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("test.toml"), "value = \"global\"\n").unwrap();

        // Set up project config
        let project_tmp = TempDir::new().unwrap();
        let tau_dir = project_tmp.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(tau_dir.join("test.toml"), "value = \"project\"\n").unwrap();

        // Set up operator config
        let operator_dir = global_dir.join("projects").join("testproj");
        fs::create_dir_all(&operator_dir).unwrap();
        fs::write(operator_dir.join("test.toml"), "value = \"operator\"\n").unwrap();

        let results: Vec<(PathBuf, SimpleConfig)> = load_all(
            Some("testproj"),
            Some(project_tmp.path().to_str().unwrap()),
            "test.toml",
            true,
        );

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].1.value.as_deref(), Some("operator"));
        assert_eq!(results[1].1.value.as_deref(), Some("project"));
        assert_eq!(results[2].1.value.as_deref(), Some("global"));
    }

    #[test]
    fn load_all_skips_missing_files() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("test.toml"), "value = \"global\"\n").unwrap();

        // No project or operator config
        let results: Vec<(PathBuf, SimpleConfig)> = load_all(
            Some("testproj"),
            Some("/nonexistent/project"),
            "test.toml",
            true,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.value.as_deref(), Some("global"));
    }

    #[test]
    fn load_all_skips_malformed_toml() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("test.toml"), "value = \"global\"\n").unwrap();

        // Malformed project config
        let project_tmp = TempDir::new().unwrap();
        let tau_dir = project_tmp.path().join(".tau");
        fs::create_dir_all(&tau_dir).unwrap();
        fs::write(tau_dir.join("test.toml"), "this is not [[ valid toml }{").unwrap();

        let results: Vec<(PathBuf, SimpleConfig)> = load_all(
            None,
            Some(project_tmp.path().to_str().unwrap()),
            "test.toml",
            true,
        );

        // Only global should be loaded (malformed project tier is skipped)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.value.as_deref(), Some("global"));
    }

    // --- load_first tests ---

    #[test]
    fn load_first_returns_highest_priority() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("test.toml"), "value = \"global\"\n").unwrap();

        // Operator config
        let operator_dir = global_dir.join("projects").join("testproj");
        fs::create_dir_all(&operator_dir).unwrap();
        fs::write(operator_dir.join("test.toml"), "value = \"operator\"\n").unwrap();

        let result: Option<SimpleConfig> =
            load_first(Some("testproj"), Some("/nonexistent"), "test.toml", true);

        assert_eq!(result.unwrap().value.as_deref(), Some("operator"));
    }

    #[test]
    fn load_first_falls_back_to_global() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("test.toml"), "value = \"global\"\n").unwrap();

        let result: Option<SimpleConfig> = load_first(None, None, "test.toml", true);

        assert_eq!(result.unwrap().value.as_deref(), Some("global"));
    }

    #[test]
    fn load_first_returns_none_when_no_file() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let config_tmp = TempDir::new().unwrap();
        let _xdg = set_xdg(config_tmp.path());
        // Don't create any files

        let result: Option<SimpleConfig> = load_first(None, None, "test.toml", true);

        assert!(result.is_none());
    }
}
