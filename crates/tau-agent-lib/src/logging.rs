//! Tracing initialisation for the tau server daemon.
//!
//! Log output goes to a daily-rotated file in `paths::logs_dir()`.
//! Env filter: `TAU_LOG` wins, then `RUST_LOG`, else `"info"`.
//!
//! Old log files (>14 days) are cleaned up on boot.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// How many days of log files to keep. Older files are deleted on boot.
const LOG_RETENTION_DAYS: u64 = 14;

/// Initialise tracing for the tau server daemon.
///
/// Output: rolling daily-rotated file in `paths::logs_dir()`, one file per day,
/// files older than 14 days are deleted on boot.
///
/// Env filter: reads `TAU_LOG` if set, falling back to `RUST_LOG`, falling back
/// to `"info"`. Standard `tracing-subscriber` env-filter syntax
/// (e.g. `TAU_LOG=tau_agent_lib::plugin=debug,tau_agent_lib=info`).
///
/// Returns a `WorkerGuard` that must be kept alive for the process lifetime
/// so the non-blocking appender drains on shutdown.
pub fn init_tracing() -> WorkerGuard {
    let logs_dir = tau_agent_base::paths::logs_dir();
    let _ = fs::create_dir_all(&logs_dir);

    // Best-effort cleanup of old log files.
    cleanup_old_logs(&logs_dir, LOG_RETENTION_DAYS);

    let file_appender = tracing_appender::rolling::daily(&logs_dir, "server.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("TAU_LOG")
        .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::registry().with(filter).with(
        fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true),
    );

    // Use `try_init` so that tests (which may instantiate multiple
    // subscribers across runs) don't panic on a double-install. If this
    // fails in production it means someone else already installed a
    // subscriber — log that fact to stderr (which the parent daemon
    // redirects to `daemon.stderr.log`) so the silent-logging regression
    // is at least discoverable.
    if let Err(e) = subscriber.try_init() {
        eprintln!("tau: failed to install tracing subscriber: {e}");
    }

    guard
}

/// Delete log files in `dir` whose mtime is older than `max_age_days`.
///
/// Only considers files whose name starts with "server.log" — leaves unrelated
/// files alone. Non-fatal: all errors are silently ignored.
pub fn cleanup_old_logs(dir: &Path, max_age_days: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let max_age = Duration::from_secs(max_age_days * 24 * 60 * 60);
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only clean our own log files. `tracing-appender::rolling::daily`
        // produces `server.log.YYYY-MM-DD` names.
        if !name.starts_with("server.log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if let Ok(age) = now.duration_since(mtime)
            && age > max_age
        {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn cleanup_removes_files_older_than_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("server.log.2001-01-01");
        let recent = dir.path().join("server.log.2999-01-01");
        let unrelated = dir.path().join("something-else.log");
        for p in [&old, &recent, &unrelated] {
            let mut f = File::create(p).expect("create");
            writeln!(f, "x").expect("write");
        }
        // Set old file's mtime 30 days ago.
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        let old_f = File::open(&old).expect("open old");
        old_f
            .set_modified(thirty_days_ago)
            .expect("set_modified old");
        drop(old_f);
        let unrelated_f = File::open(&unrelated).expect("open unrelated");
        unrelated_f
            .set_modified(thirty_days_ago)
            .expect("set_modified unrelated");
        drop(unrelated_f);

        cleanup_old_logs(dir.path(), 14);

        assert!(!old.exists(), "old log file should be deleted");
        assert!(recent.exists(), "recent log file should survive");
        assert!(unrelated.exists(), "unrelated files should not be touched");
    }

    #[test]
    fn cleanup_is_silent_when_dir_missing() {
        let missing = std::path::Path::new("/nonexistent/tau-test-dir");
        // Just making sure this doesn't panic.
        cleanup_old_logs(missing, 14);
    }

    #[test]
    fn init_tracing_writes_to_logs_dir_and_filters() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        // Point XDG_STATE_HOME at our tempdir so logs_dir() resolves inside it.
        let prev_state = std::env::var("XDG_STATE_HOME").ok();
        let prev_log = std::env::var("TAU_LOG").ok();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", dir.path());
            std::env::set_var("TAU_LOG", "warn");
        }

        let guard = init_tracing();
        tracing::info!("should be filtered out");
        tracing::warn!("should appear");
        drop(guard);

        // Allow the appender to drain.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let logs = dir.path().join("tau").join("logs");
        let mut found = false;
        let mut content = String::new();
        if let Ok(entries) = std::fs::read_dir(&logs) {
            for entry in entries.flatten() {
                if let Ok(c) = std::fs::read_to_string(entry.path()) {
                    content.push_str(&c);
                    found = true;
                }
            }
        }
        assert!(found, "expected a server.log file under {:?}", logs);
        assert!(
            content.contains("should appear"),
            "warn-level message should be present; got:\n{content}"
        );
        // With TAU_LOG=warn, info messages must be filtered.
        assert!(
            !content.contains("should be filtered out"),
            "info-level message should not appear under TAU_LOG=warn; got:\n{content}"
        );

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            match prev_log {
                Some(v) => std::env::set_var("TAU_LOG", v),
                None => std::env::remove_var("TAU_LOG"),
            }
        }
    }
}
