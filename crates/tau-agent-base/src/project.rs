//! Project configuration, discovery, and initialization.
//!
//! A tau project is identified by a `.tau/project.toml` file at its root.
//! This module provides helpers to validate project names, discover existing
//! projects by walking up the directory tree, and initialize new ones.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Persisted project configuration stored in `.tau/project.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
}

/// In-memory handle to a discovered or initialized project.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub name: String,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Validation & helpers
// ---------------------------------------------------------------------------

/// Maximum length for a project name.
const MAX_NAME_LEN: usize = 64;

/// Validate that `name` is a legal project name.
///
/// Rules:
/// - Matches `^[a-z0-9][a-z0-9_-]*$`
/// - At most 64 characters
pub fn validate_project_name(name: &str) -> crate::Result<()> {
    if name.is_empty() {
        return Err(crate::Error::Io("project name must not be empty".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(crate::Error::Io(format!(
            "project name exceeds {MAX_NAME_LEN} characters"
        )));
    }

    let bytes = name.as_bytes();

    // First character: [a-z0-9]
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(crate::Error::Io(
            "project name must start with a lowercase letter or digit".into(),
        ));
    }

    // Remaining characters: [a-z0-9_-]
    for &b in &bytes[1..] {
        if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'_' && b != b'-' {
            return Err(crate::Error::Io(format!(
                "project name contains invalid character '{}'",
                b as char,
            )));
        }
    }

    Ok(())
}

/// Convert a directory name (or arbitrary string) into a valid project name.
///
/// - Converts to lowercase
/// - Replaces any character outside `[a-z0-9_-]` with `-`
/// - Collapses consecutive `-` into one
/// - Strips leading non-`[a-z0-9]` characters
/// - Truncates to [`MAX_NAME_LEN`]
/// - Falls back to `"project"` if the result would be empty
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());

    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' || lower == '-' {
            slug.push(lower);
        } else {
            slug.push('-');
        }
    }

    // Collapse consecutive dashes.
    let mut collapsed = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for ch in slug.chars() {
        if ch == '-' {
            if !prev_dash {
                collapsed.push(ch);
            }
            prev_dash = true;
        } else {
            prev_dash = false;
            collapsed.push(ch);
        }
    }

    // Strip leading characters that are not [a-z0-9].
    let trimmed =
        collapsed.trim_start_matches(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit());

    let mut result = trimmed.to_string();

    // Truncate to max length.
    if result.len() > MAX_NAME_LEN {
        result.truncate(MAX_NAME_LEN);
        // Don't leave a trailing dash after truncation.
        result = result.trim_end_matches('-').to_string();
    }

    if result.is_empty() {
        return "project".to_string();
    }

    result
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Walk up from `start` looking for `.tau/project.toml`.
///
/// Returns `(project_name, canonicalized_project_root)` if found.
pub fn discover_project(start: &Path) -> Option<(String, PathBuf)> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };

    loop {
        let config_path = dir.join(".tau").join("project.toml");
        if config_path.is_file() {
            let contents = match std::fs::read_to_string(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        path = %config_path.display(),
                        error = %e,
                        "discover_project: failed to read project.toml",
                    );
                    return None;
                }
            };
            let config: ProjectConfig = match toml::from_str(&contents) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        path = %config_path.display(),
                        error = %e,
                        "discover_project: malformed project.toml",
                    );
                    return None;
                }
            };
            let canonical = match dir.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %e,
                        "discover_project: failed to canonicalize project root",
                    );
                    return None;
                }
            };
            return Some((config.name, canonical));
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize a new tau project at `path` with the given `name`.
///
/// Creates:
/// - `<path>/.tau/project.toml` with the project config
/// - `<path>/.tau/.gitignore` containing `/worktrees/`
/// - Operator config dir at `<config_dir>/projects/<name>/`
///
/// Returns the canonicalized project root path.
pub fn init_project(path: &Path, name: &str) -> crate::Result<PathBuf> {
    validate_project_name(name)?;

    let tau_dir = path.join(".tau");
    let config_path = tau_dir.join("project.toml");

    if config_path.exists() {
        return Err(crate::Error::Io(format!(
            "project already initialized: {} exists",
            config_path.display(),
        )));
    }

    // Create .tau/ directory.
    std::fs::create_dir_all(&tau_dir).map_err(|e| crate::Error::Io(e.to_string()))?;

    // Write project.toml.
    let config = ProjectConfig {
        name: name.to_string(),
    };
    let toml_content =
        toml::to_string_pretty(&config).map_err(|e| crate::Error::Io(e.to_string()))?;
    std::fs::write(&config_path, toml_content).map_err(|e| crate::Error::Io(e.to_string()))?;

    // Write or update .gitignore.
    let gitignore_path = tau_dir.join(".gitignore");
    let worktrees_line = "/worktrees/";
    if gitignore_path.exists() {
        let existing = std::fs::read_to_string(&gitignore_path)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        if !existing.lines().any(|line| line.trim() == worktrees_line) {
            let mut content = existing;
            if !content.ends_with('\n') && !content.is_empty() {
                content.push('\n');
            }
            content.push_str(worktrees_line);
            content.push('\n');
            std::fs::write(&gitignore_path, content)
                .map_err(|e| crate::Error::Io(e.to_string()))?;
        }
    } else {
        std::fs::write(&gitignore_path, format!("{worktrees_line}\n"))
            .map_err(|e| crate::Error::Io(e.to_string()))?;
    }

    // Create operator config directory.
    let operator_dir = crate::paths::config_dir().join("projects").join(name);
    std::fs::create_dir_all(&operator_dir).map_err(|e| crate::Error::Io(e.to_string()))?;

    // Return canonicalized project root.
    path.canonicalize()
        .map_err(|e| crate::Error::Io(e.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_project_name ------------------------------------------------

    #[test]
    fn valid_names() {
        assert!(validate_project_name("a").is_ok());
        assert!(validate_project_name("abc").is_ok());
        assert!(validate_project_name("my-project").is_ok());
        assert!(validate_project_name("my_project").is_ok());
        assert!(validate_project_name("0cool").is_ok());
        assert!(validate_project_name("a1-b2_c3").is_ok());
    }

    #[test]
    fn invalid_empty() {
        assert!(validate_project_name("").is_err());
    }

    #[test]
    fn invalid_too_long() {
        let long = "a".repeat(65);
        assert!(validate_project_name(&long).is_err());
        // Exactly 64 is fine.
        let exact = "a".repeat(64);
        assert!(validate_project_name(&exact).is_ok());
    }

    #[test]
    fn invalid_start_char() {
        assert!(validate_project_name("-foo").is_err());
        assert!(validate_project_name("_foo").is_err());
        assert!(validate_project_name(".foo").is_err());
    }

    #[test]
    fn invalid_uppercase() {
        assert!(validate_project_name("Foo").is_err());
        assert!(validate_project_name("fOo").is_err());
    }

    #[test]
    fn invalid_special_chars() {
        assert!(validate_project_name("foo bar").is_err());
        assert!(validate_project_name("foo.bar").is_err());
        assert!(validate_project_name("foo/bar").is_err());
    }

    // -- slugify --------------------------------------------------------------

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("MyProject"), "myproject");
    }

    #[test]
    fn slugify_spaces_and_dots() {
        assert_eq!(slugify("My Cool Project"), "my-cool-project");
        assert_eq!(slugify("foo.bar.baz"), "foo-bar-baz");
    }

    #[test]
    fn slugify_leading_invalid() {
        assert_eq!(slugify("--foo"), "foo");
        assert_eq!(slugify("__bar"), "bar");
        assert_eq!(slugify("...baz"), "baz");
    }

    #[test]
    fn slugify_collapse_dashes() {
        assert_eq!(slugify("a---b"), "a-b");
    }

    #[test]
    fn slugify_empty_fallback() {
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("..."), "project");
    }

    #[test]
    fn slugify_truncate() {
        let long = "a".repeat(100);
        let result = slugify(&long);
        assert!(result.len() <= MAX_NAME_LEN);
        assert!(validate_project_name(&result).is_ok());
    }

    #[test]
    fn slugify_result_is_valid() {
        let cases = ["My Project", "foo/bar", "__init__", "CamelCase123"];
        for input in &cases {
            let s = slugify(input);
            assert!(
                validate_project_name(&s).is_ok(),
                "slugify({input:?}) = {s:?} failed validation",
            );
        }
    }

    // -- discover_project -----------------------------------------------------

    // Tests that modify environment variables must be serialized to avoid
    // races with the default parallel test runner.

    #[test]
    fn discover_finds_project_at_root() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();

        // Set up .tau/project.toml
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"test-proj\"\n").unwrap();

        let (name, found_path) = discover_project(root).expect("should discover");
        assert_eq!(name, "test-proj");
        assert_eq!(found_path, root.canonicalize().unwrap());
    }

    #[test]
    fn discover_walks_up() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();

        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"walk-up\"\n").unwrap();

        // Create a nested directory.
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();

        let (name, found_path) = discover_project(&nested).expect("should discover");
        assert_eq!(name, "walk-up");
        assert_eq!(found_path, root.canonicalize().unwrap());
    }

    #[test]
    fn discover_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        assert!(discover_project(tmp.path()).is_none());
    }

    #[test]
    fn discover_with_trailing_slash() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"trailing\"\n").unwrap();

        // Append a trailing slash to the path.
        let mut with_slash = root.to_string_lossy().into_owned();
        with_slash.push('/');
        let p = std::path::Path::new(&with_slash);

        let (name, found) = discover_project(p).expect("should discover");
        assert_eq!(name, "trailing");
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn discover_with_dot_dot_in_path() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"dotdot\"\n").unwrap();

        // Build a path of the form `<root>/sub/..` that resolves to root.
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let weird = sub.join("..");

        let (name, found) = discover_project(&weird).expect("should discover via .. path");
        assert_eq!(name, "dotdot");
        // Canonicalisation collapses `..` so the discovered root matches the real one.
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn discover_nonexistent_path_returns_none() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let bogus = tmp.path().join("does").join("not").join("exist");
        // Must not panic and must return None.
        assert!(discover_project(&bogus).is_none());
    }

    #[test]
    fn discover_malformed_toml_returns_none() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        // Not valid TOML (missing quotes / value).
        std::fs::write(tau_dir.join("project.toml"), "name = \n").unwrap();

        assert!(discover_project(root).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn discover_via_symlink_to_project_root() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path().join("real");
        std::fs::create_dir_all(&root).unwrap();
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("project.toml"), "name = \"sym\"\n").unwrap();

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();

        let (name, found) = discover_project(&link).expect("should discover via symlink");
        assert_eq!(name, "sym");
        // Canonicalised path resolves through the symlink to the real dir.
        assert_eq!(found, root.canonicalize().unwrap());
    }

    // -- init_project ---------------------------------------------------------

    #[test]
    fn init_creates_files() {
        let _lock = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();

        // Override config dir so we don't touch the real home.
        let config_tmp = tempfile::tempdir().expect("create config tempdir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_tmp.path()) };

        let operator_dir = crate::paths::config_dir()
            .join("projects")
            .join("test-init");
        let result = init_project(root, "test-init");

        let canonical = result.expect("init_project should succeed");
        assert_eq!(canonical, root.canonicalize().unwrap());

        // .tau/project.toml exists and is valid.
        let toml_content = std::fs::read_to_string(root.join(".tau").join("project.toml")).unwrap();
        let config: ProjectConfig = toml::from_str(&toml_content).unwrap();
        assert_eq!(config.name, "test-init");

        // .tau/.gitignore contains /worktrees/.
        let gitignore = std::fs::read_to_string(root.join(".tau").join(".gitignore")).unwrap();
        assert!(gitignore.contains("/worktrees/"));

        // Operator config dir was created.
        assert!(operator_dir.is_dir());

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn init_rejects_invalid_name() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        assert!(init_project(tmp.path(), "Bad Name!").is_err());
    }

    #[test]
    fn init_rejects_duplicate() {
        let _lock = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();

        let config_tmp = tempfile::tempdir().expect("create config tempdir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_tmp.path()) };

        init_project(root, "dup-test").expect("first init should succeed");

        let err = init_project(root, "dup-test");
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        assert!(err.is_err());
    }

    #[test]
    fn init_gitignore_no_duplicate_line() {
        let _lock = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();

        // Pre-create .tau/.gitignore with the line already present.
        let tau_dir = root.join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join(".gitignore"), "/worktrees/\n").unwrap();

        let config_tmp = tempfile::tempdir().expect("create config tempdir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_tmp.path()) };

        init_project(root, "gi-test").expect("init should succeed");

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        let gitignore = std::fs::read_to_string(tau_dir.join(".gitignore")).unwrap();
        let count = gitignore
            .lines()
            .filter(|l| l.trim() == "/worktrees/")
            .count();
        assert_eq!(count, 1, "should not duplicate /worktrees/ line");
    }
}
