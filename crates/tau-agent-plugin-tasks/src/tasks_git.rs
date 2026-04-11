//! Git utility functions for task branch and worktree management.
//!
//! These are pure helper functions that run git commands via `std::process::Command`
//! and return results. They are used by the plugin process for git operations.

use std::path::Path;
use std::process::Command;

/// Create a new git branch from a base branch.
pub fn create_branch(
    repo_path: &str,
    branch_name: &str,
    base_branch: &str,
) -> tau_agent_plugin::Result<()> {
    let output = Command::new("git")
        .args(["branch", branch_name, base_branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git branch: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git branch {} from {}: {}",
            branch_name,
            base_branch,
            stderr.trim()
        )));
    }
    Ok(())
}

/// Create a new git worktree at `worktree_path` checked out to `branch_name`.
pub fn create_worktree(
    repo_path: &str,
    worktree_path: &str,
    branch_name: &str,
) -> tau_agent_plugin::Result<()> {
    let output = Command::new("git")
        .args(["worktree", "add", worktree_path, branch_name])
        .current_dir(repo_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git worktree add: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git worktree add {} {}: {}",
            worktree_path,
            branch_name,
            stderr.trim()
        )));
    }
    Ok(())
}

/// Remove a git worktree. Uses `--force` to handle dirty worktrees.
///
/// Safety: refuses to remove the main worktree (repo root) to prevent
/// accidental deletion of the primary working tree.
pub fn remove_worktree(repo_path: &str, worktree_path: &str) -> tau_agent_plugin::Result<()> {
    // Guard: never remove the main worktree.
    let repo_canon = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.into());
    let wt_canon = std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.into());
    if repo_canon == wt_canon {
        return Err(tau_agent_plugin::Error::Io(format!(
            "refusing to remove main worktree: {} is the repo root",
            worktree_path
        )));
    }

    let output = Command::new("git")
        .args(["worktree", "remove", "--force", worktree_path])
        .current_dir(repo_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git worktree remove: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git worktree remove {}: {}",
            worktree_path,
            stderr.trim()
        )));
    }
    Ok(())
}

/// Delete a local git branch. Uses `-D` to force-delete even if not merged.
pub fn delete_branch(repo_path: &str, branch_name: &str) -> tau_agent_plugin::Result<()> {
    let output = Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(repo_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git branch -D: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git branch -D {}: {}",
            branch_name,
            stderr.trim()
        )));
    }
    Ok(())
}

/// Get the repository root directory from a path inside the repo.
pub fn get_repo_root(cwd: &str) -> tau_agent_plugin::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|e| {
            tau_agent_plugin::Error::Io(format!("git rev-parse --show-toplevel: {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git rev-parse --show-toplevel in {}: {}",
            cwd,
            stderr.trim()
        )));
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(root)
}

/// Check whether a branch exists in the repository.
pub fn branch_exists(repo_path: &str, branch_name: &str) -> tau_agent_plugin::Result<bool> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            &format!("refs/heads/{}", branch_name),
        ])
        .current_dir(repo_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git rev-parse --verify: {}", e)))?;

    Ok(output.status.success())
}

/// Derive the branch name for a task based on its id and optional parent_id.
///
/// - Root tasks: `task-{id}`
/// - Subtasks: `task-{parent_id}-{id}`
pub fn task_branch_name(task_id: i64, parent_id: Option<i64>) -> String {
    match parent_id {
        Some(pid) => format!("task-{}-{}", pid, task_id),
        None => format!("task-{}", task_id),
    }
}

/// Derive the worktree path for a task given the repo root.
///
/// If repo is at `/home/user/src/tau`, returns `/home/user/src/tau-task-{id}`.
pub fn task_worktree_path(repo_root: &str, task_id: i64) -> tau_agent_plugin::Result<String> {
    let root = Path::new(repo_root);
    let repo_name = root.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("cannot derive repo name from path: {}", repo_root))
    })?;
    let parent = root.parent().ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("repo root has no parent directory: {}", repo_root))
    })?;
    let worktree_dir = format!("{}-task-{}", repo_name, task_id);
    Ok(parent.join(worktree_dir).to_string_lossy().into_owned())
}

/// Abort a partial rebase if one is in progress in the given worktree.
///
/// Uses `git rev-parse --git-dir` to find the actual git directory (which differs
/// for worktrees), then checks for `rebase-merge` or `rebase-apply` directories.
/// If either exists, runs `git rebase --abort`. Always returns `Ok(())` if no
/// partial rebase is found.
pub fn abort_partial_rebase(worktree_path: &str) -> tau_agent_plugin::Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree_path)
        .output()
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git rev-parse --git-dir: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tau_agent_plugin::Error::Io(format!(
            "git rev-parse --git-dir in {}: {}",
            worktree_path,
            stderr.trim()
        )));
    }

    let git_dir_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_dir = if Path::new(&git_dir_raw).is_absolute() {
        std::path::PathBuf::from(&git_dir_raw)
    } else {
        Path::new(worktree_path).join(&git_dir_raw)
    };

    let has_rebase = git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();

    if has_rebase {
        let output = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(worktree_path)
            .output()
            .map_err(|e| tau_agent_plugin::Error::Io(format!("git rebase --abort: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(tau_agent_plugin::Error::Io(format!(
                "git rebase --abort in {}: {}",
                worktree_path,
                stderr.trim()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test git repo with an initial commit and a "main" branch.
    fn init_test_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .output()
            .expect("git init");

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .expect("git config email");

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .expect("git config name");

        // Create an initial commit so "main" branch exists
        let file = path.join("README.md");
        std::fs::write(&file, "# test\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .expect("git add");

        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(path)
            .output()
            .expect("git commit");

        dir
    }

    #[test]
    fn test_get_repo_root() {
        let dir = init_test_repo();
        let root = get_repo_root(dir.path().to_str().unwrap()).unwrap();
        // Canonicalize both for comparison (handles symlinks like /tmp -> /private/tmp on macOS)
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        let actual = std::fs::canonicalize(&root).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_get_repo_root_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = get_repo_root(dir.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_branch_exists() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        assert!(branch_exists(path, "main").unwrap());
        assert!(!branch_exists(path, "nonexistent").unwrap());
    }

    #[test]
    fn test_create_branch() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        create_branch(path, "task-5", "main").unwrap();
        assert!(branch_exists(path, "task-5").unwrap());
    }

    #[test]
    fn test_create_branch_from_nonexistent_base() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        let result = create_branch(path, "task-5", "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_branch_duplicate() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        create_branch(path, "task-5", "main").unwrap();
        let result = create_branch(path, "task-5", "main");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_and_remove_worktree() {
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Create a branch first
        create_branch(repo_path, "task-1", "main").unwrap();

        // Create worktree
        let wt_path = dir.path().parent().unwrap().join("test-worktree");
        let wt_str = wt_path.to_str().unwrap();

        create_worktree(repo_path, wt_str, "task-1").unwrap();
        assert!(wt_path.exists());

        // The worktree should have the repo contents
        assert!(wt_path.join("README.md").exists());

        // Remove worktree
        remove_worktree(repo_path, wt_str).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_create_worktree_nonexistent_branch() {
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        let wt_path = dir.path().parent().unwrap().join("bad-worktree");
        let wt_str = wt_path.to_str().unwrap();

        let result = create_worktree(repo_path, wt_str, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_nonexistent_worktree() {
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        let result = remove_worktree(repo_path, "/tmp/does-not-exist-worktree");
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_worktree_refuses_main_worktree() {
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Trying to remove the repo root itself should be refused
        let result = remove_worktree(repo_path, repo_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("refusing to remove main worktree"),
            "expected 'refusing to remove main worktree' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_task_branch_name() {
        assert_eq!(task_branch_name(5, None), "task-5");
        assert_eq!(task_branch_name(1, Some(5)), "task-5-1");
        assert_eq!(task_branch_name(42, Some(10)), "task-10-42");
    }

    #[test]
    fn test_task_worktree_path() {
        let path = task_worktree_path("/home/kaspar/src/ai/tau", 5).unwrap();
        assert_eq!(path, "/home/kaspar/src/ai/tau-task-5");

        let path = task_worktree_path("/home/user/project", 42).unwrap();
        assert_eq!(path, "/home/user/project-task-42");
    }

    #[test]
    fn test_task_worktree_path_root() {
        // Edge case: repo at filesystem root-like path
        let result = task_worktree_path("/", 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_end_to_end_branch_and_worktree() {
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Simulate creating a root task branch + worktree
        let task_id = 5;
        let branch = task_branch_name(task_id, None);
        assert_eq!(branch, "task-5");

        create_branch(repo_path, &branch, "main").unwrap();
        assert!(branch_exists(repo_path, &branch).unwrap());

        let wt_path = task_worktree_path(repo_path, task_id).unwrap();
        create_worktree(repo_path, &wt_path, &branch).unwrap();
        assert!(Path::new(&wt_path).exists());

        // Now create a subtask branch forked from parent
        let subtask_id = 1;
        let sub_branch = task_branch_name(subtask_id, Some(task_id));
        assert_eq!(sub_branch, "task-5-1");

        create_branch(repo_path, &sub_branch, &branch).unwrap();
        assert!(branch_exists(repo_path, &sub_branch).unwrap());

        let sub_wt_path = task_worktree_path(repo_path, subtask_id).unwrap();
        create_worktree(repo_path, &sub_wt_path, &sub_branch).unwrap();
        assert!(Path::new(&sub_wt_path).exists());

        // Cleanup
        remove_worktree(repo_path, &sub_wt_path).unwrap();
        remove_worktree(repo_path, &wt_path).unwrap();
        assert!(!Path::new(&sub_wt_path).exists());
        assert!(!Path::new(&wt_path).exists());
    }

    #[test]
    fn test_delete_branch() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        create_branch(path, "to-delete", "main").unwrap();
        assert!(branch_exists(path, "to-delete").unwrap());

        delete_branch(path, "to-delete").unwrap();
        assert!(!branch_exists(path, "to-delete").unwrap());
    }

    #[test]
    fn test_delete_branch_nonexistent() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        let result = delete_branch(path, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_abort_partial_rebase_no_rebase() {
        let dir = init_test_repo();
        let path = dir.path().to_str().unwrap();

        // Should return Ok even when no rebase is in progress
        abort_partial_rebase(path).unwrap();
    }
}
