//! Shared XDG path resolution for tau directories.
//!
//! When `$HOME` is unset (e.g. in containers or minimal environments), the
//! fallback paths use subdirectories under `/tmp`:
//!
//! - config: `/tmp/tau-config/`
//! - data:   `/tmp/tau-data/`
//! - runtime: `/tmp/tau-{PID}/`
//!
//! This differs from the pre-extraction per-file fallbacks (e.g.
//! `/tmp/tau-auth.json`, `/tmp/tau.db`) which were flat files in `/tmp`.
//! The subdirectory approach is intentional: it keeps tau files namespaced
//! under their own directories even in the fallback case, avoids collisions
//! with unrelated files, and allows `create_dir_all` to work uniformly
//! (callers always join a filename onto a directory).

use std::path::PathBuf;

/// Returns the tau config directory (`$XDG_CONFIG_HOME/tau` or `~/.config/tau`).
///
/// Fallback when `$HOME` is unset: `/tmp/tau-config`.
pub fn config_dir() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("tau")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("tau")
    } else {
        PathBuf::from("/tmp").join("tau-config")
    }
}

/// Returns the tau data directory (`$XDG_DATA_HOME/tau` or `~/.local/share/tau`).
///
/// Fallback when `$HOME` is unset: `/tmp/tau-data`.
pub fn data_dir() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data).join("tau")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("share").join("tau")
    } else {
        PathBuf::from("/tmp").join("tau-data")
    }
}

/// Returns the tau runtime directory (`$XDG_RUNTIME_DIR/tau` or `~/.tau`).
///
/// Fallback when `$HOME` is unset: `/tmp/tau-{PID}` (per-process).
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("tau")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".tau")
    } else {
        PathBuf::from("/tmp").join(format!("tau-{}", std::process::id()))
    }
}
