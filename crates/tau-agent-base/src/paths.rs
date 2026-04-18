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

/// Returns the tau state directory (`$XDG_STATE_HOME/tau` or `~/.local/state/tau`).
///
/// Fallback when `$HOME` is unset: `/tmp/tau-state`.
///
/// State directory is for data that survives restarts but is not
/// user-editable config: logs, crash dumps, internal checkpoints.
pub fn state_dir() -> PathBuf {
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(state).join("tau")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("state").join("tau")
    } else {
        PathBuf::from("/tmp").join("tau-state")
    }
}

/// Returns the tau logs directory (`state_dir()/logs`).
pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Returns the default socket path for the tau server.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("tau.sock")
}

/// Returns the PID file path next to the socket.
pub fn pid_path() -> PathBuf {
    let mut p = socket_path();
    p.set_file_name("tau.pid");
    p
}

/// Returns the operator config directory for a project.
///
/// `~/.config/tau/projects/{name}/`
pub fn project_config_dir(name: &str) -> PathBuf {
    config_dir().join("projects").join(name)
}

/// Check if a server is already running by trying to connect.
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests in this module mutate process-global env vars; serialize them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvSnapshot {
        xdg_state: Option<String>,
        xdg_config: Option<String>,
        xdg_data: Option<String>,
        xdg_runtime: Option<String>,
        home: Option<String>,
    }

    impl EnvSnapshot {
        fn capture() -> Self {
            Self {
                xdg_state: std::env::var("XDG_STATE_HOME").ok(),
                xdg_config: std::env::var("XDG_CONFIG_HOME").ok(),
                xdg_data: std::env::var("XDG_DATA_HOME").ok(),
                xdg_runtime: std::env::var("XDG_RUNTIME_DIR").ok(),
                home: std::env::var("HOME").ok(),
            }
        }

        fn restore(self) {
            fn set(k: &str, v: Option<String>) {
                // SAFETY: serialized by ENV_LOCK; no other threads should be
                // touching these env vars for the duration of a test.
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
            set("XDG_STATE_HOME", self.xdg_state);
            set("XDG_CONFIG_HOME", self.xdg_config);
            set("XDG_DATA_HOME", self.xdg_data);
            set("XDG_RUNTIME_DIR", self.xdg_runtime);
            set("HOME", self.home);
        }
    }

    fn set_var(k: &str, v: &str) {
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var(k, v);
        }
    }

    fn remove_var(k: &str) {
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn state_dir_uses_xdg_state_home_when_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snap = EnvSnapshot::capture();
        set_var("XDG_STATE_HOME", "/custom/state");
        set_var("HOME", "/home/ignored");
        assert_eq!(state_dir(), PathBuf::from("/custom/state/tau"));
        assert_eq!(logs_dir(), PathBuf::from("/custom/state/tau/logs"));
        snap.restore();
    }

    #[test]
    fn state_dir_uses_home_when_xdg_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snap = EnvSnapshot::capture();
        remove_var("XDG_STATE_HOME");
        set_var("HOME", "/home/alice");
        assert_eq!(state_dir(), PathBuf::from("/home/alice/.local/state/tau"));
        assert_eq!(
            logs_dir(),
            PathBuf::from("/home/alice/.local/state/tau/logs")
        );
        snap.restore();
    }

    #[test]
    fn state_dir_falls_back_to_tmp_when_neither_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snap = EnvSnapshot::capture();
        remove_var("XDG_STATE_HOME");
        remove_var("HOME");
        assert_eq!(state_dir(), PathBuf::from("/tmp/tau-state"));
        assert_eq!(logs_dir(), PathBuf::from("/tmp/tau-state/logs"));
        snap.restore();
    }
}
