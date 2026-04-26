//! Validation for task `affected_files` paths.
//!
//! Tasks live inside a single project: the scheduler creates one branch and
//! one worktree under that project's repo, and the merge step ff-merges that
//! branch into the project's merge target. If a task's `affected_files`
//! list contains paths that resolve **outside** the project root, the
//! agent will edit files in a sibling repo while the task's own branch
//! sits empty — and the merge will silently succeed with no real changes.
//! See task #749 for the full bug write-up and the recovery walks it caused.
//!
//! This module exposes a single helper, [`validate_affected_files`], that
//! both the create-time path (`handle_task_create`) and the planner-write
//! path (`handle_task_update`) call before persisting the list.
//!
//! The check is **lexical**: we never call `std::fs::canonicalize`, because
//! the listed paths frequently don't exist yet (new tasks file paths they
//! intend to create). Instead we resolve `.` and `..` purely on path
//! components and verify the result still lives under the project root.

use std::path::{Component, Path, PathBuf};

/// The "I touch everything / unknowable" escape hatch. Carried in
/// `affected_files` as the single string `"*"`. Not a path; the
/// validator must not try to resolve it.
const STAR_MARKER: &str = "*";

/// Validate every entry in an `affected_files` list against the given
/// project root.
///
/// Rules:
/// - The single-element marker `["*"]` is always accepted (it's not a path).
/// - Empty list is accepted (no paths to validate).
/// - Absolute paths are rejected — `affected_files` is project-relative
///   by convention and absolute paths bypass the relative-path check.
/// - Each entry is joined to `project_root`, lexically normalised
///   (resolving `.` and `..` on components), and the result must remain
///   inside `project_root`. `..` segments that climb above the root are
///   rejected.
///
/// On rejection, returns a human-readable error string naming the
/// offending entry and the project root, so the caller can surface it
/// to the user / planner.
pub(crate) fn validate_affected_files(
    entries: &[String],
    project_root: &Path,
) -> Result<(), String> {
    // The `["*"]` escape hatch is dispatched before path checking — it
    // means "this task touches everything / can't predict its file set".
    if entries.len() == 1 && entries[0] == STAR_MARKER {
        return Ok(());
    }

    for entry in entries {
        // `*` only has meaning as the sole element of the list. Inside a
        // larger list it's almost certainly a mistake, but treating it
        // as a path would be even more confusing — reject explicitly.
        if entry == STAR_MARKER {
            return Err(format!(
                "affected_files entry '*' is only valid as the sole entry of the list \
                 (the \"touches everything\" marker). Mixed with concrete paths it has \
                 no defined meaning."
            ));
        }

        let entry_path = Path::new(entry);
        if entry_path.is_absolute() {
            return Err(format!(
                "affected_files entry '{}' is an absolute path. \
                 affected_files must be project-relative paths inside '{}'.",
                entry,
                project_root.display(),
            ));
        }

        // Lexically normalise `<project_root>/<entry>`.
        let joined = project_root.join(entry_path);
        let normalised = lexical_normalize(&joined);

        // `lexical_normalize` returns `None` if `..` segments climb above
        // the root of the joined path (i.e. escape).
        let normalised = match normalised {
            Some(p) => p,
            None => {
                return Err(format!(
                    "affected_files entry '{}' resolves outside project root '{}'. \
                     Tasks must be filed in the project that owns the files they touch.",
                    entry,
                    project_root.display(),
                ));
            }
        };

        // Final containment check: even if normalisation didn't fully
        // collapse, the result must start with the (also-normalised)
        // project root.
        let root_normalised =
            lexical_normalize(project_root).unwrap_or_else(|| project_root.to_path_buf());
        if !normalised.starts_with(&root_normalised) {
            return Err(format!(
                "affected_files entry '{}' resolves outside project root '{}'. \
                 Tasks must be filed in the project that owns the files they touch.",
                entry,
                project_root.display(),
            ));
        }
    }

    Ok(())
}

/// Lexically normalise a path: collapse `.` and `..` components without
/// touching the filesystem. Returns `None` if a `..` segment would climb
/// above the path's root (i.e. escape).
///
/// Behaviour notes:
/// - Preserves the absolute-vs-relative nature of the input.
/// - For absolute paths, `..` at the root is silently dropped (matches
///   POSIX `cd /; cd ..` → still at `/`).
/// - For relative paths with no prefix to anchor against, a leading
///   `..` is treated as escape and returns `None`.
fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out: Vec<Component<'_>> = Vec::new();
    let mut is_absolute = false;

    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                is_absolute = true;
                out.push(comp);
            }
            Component::CurDir => {
                // Skip `.` — pure no-op.
            }
            Component::ParentDir => {
                // Try to pop the previous non-root component.
                let can_pop = out
                    .last()
                    .map(|c| !matches!(c, Component::Prefix(_) | Component::RootDir))
                    .unwrap_or(false);
                if can_pop {
                    out.pop();
                } else if is_absolute {
                    // `cd /; cd ..` stays at `/` — drop silently.
                } else {
                    // Relative path climbing above its anchor → escape.
                    return None;
                }
            }
            Component::Normal(_) => out.push(comp),
        }
    }

    let mut result = PathBuf::new();
    for comp in &out {
        result.push(comp.as_os_str());
    }
    // An empty result for a relative path means the input collapsed to
    // the current directory — represent that as `.`.
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/kaspar/src/ai/gpuie")
    }

    fn ok(entries: &[&str]) {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        let r = validate_affected_files(&owned, &root());
        assert!(r.is_ok(), "expected ok, got: {:?}", r);
    }

    fn bad(entries: &[&str], expected_substr: &str) {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        let r = validate_affected_files(&owned, &root());
        let err = r.expect_err("expected validation error");
        assert!(
            err.contains(expected_substr),
            "error '{}' should contain '{}'",
            err,
            expected_substr
        );
    }

    #[test]
    fn empty_list_ok() {
        ok(&[]);
    }

    #[test]
    fn simple_relative_ok() {
        ok(&["src/foo.rs"]);
    }

    #[test]
    fn dot_prefixed_ok() {
        ok(&["./src/foo.rs"]);
    }

    #[test]
    fn parent_then_back_inside_ok() {
        ok(&["src/../src/foo.rs"]);
    }

    #[test]
    fn climbs_one_level_rejected() {
        bad(&["../sibling/foo.rs"], "../sibling/foo.rs");
    }

    #[test]
    fn climbs_many_levels_rejected() {
        bad(&["../../etc/passwd"], "../../etc/passwd");
    }

    #[test]
    fn absolute_path_rejected() {
        bad(&["/absolute/path"], "/absolute/path");
    }

    #[test]
    fn absolute_path_inside_root_still_rejected() {
        // Even an absolute path that happens to live inside the root is
        // rejected — the convention is project-relative.
        bad(&["/home/kaspar/src/ai/gpuie/src/foo.rs"], "absolute path");
    }

    #[test]
    fn star_marker_alone_ok() {
        ok(&["*"]);
    }

    #[test]
    fn star_in_mixed_list_rejected() {
        bad(&["src/foo.rs", "*"], "'*' is only valid as the sole entry");
    }

    #[test]
    fn mixed_list_with_one_bad_entry_names_it() {
        bad(
            &["src/good.rs", "../../../nitro/bad.rs", "src/also_good.rs"],
            "../../../nitro/bad.rs",
        );
    }

    #[test]
    fn error_message_mentions_project_root() {
        let owned = vec!["../escape.rs".to_string()];
        let err = validate_affected_files(&owned, &root()).unwrap_err();
        assert!(
            err.contains("/home/kaspar/src/ai/gpuie"),
            "error should mention the project root: {}",
            err
        );
    }

    // ---- lexical_normalize unit tests ----

    #[test]
    fn normalize_collapses_dot() {
        assert_eq!(
            lexical_normalize(Path::new("/a/./b")),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn normalize_collapses_parent() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            Some(PathBuf::from("/a/c"))
        );
    }

    #[test]
    fn normalize_root_parent_stays_at_root() {
        assert_eq!(
            lexical_normalize(Path::new("/..")),
            Some(PathBuf::from("/"))
        );
    }

    #[test]
    fn normalize_relative_escape_returns_none() {
        assert_eq!(lexical_normalize(Path::new("../x")), None);
    }

    #[test]
    fn normalize_empty_relative_becomes_dot() {
        assert_eq!(
            lexical_normalize(Path::new("a/..")),
            Some(PathBuf::from("."))
        );
    }
}
