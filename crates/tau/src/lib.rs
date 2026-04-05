pub mod agent;
pub mod auth;
pub mod client;
pub mod compaction;
pub mod config;
pub mod db;
pub mod orchestration;
pub mod plugin;
pub mod protocol;
pub mod provider;
pub mod providers;
pub mod replay;
pub mod server;
pub mod system_prompt;
pub mod tasks;
pub mod tasks_config;
pub mod tasks_db;
pub mod tasks_git;
pub mod tasks_merge;
pub mod tasks_scheduler;
pub mod throttle;
pub mod tools;
pub mod types;
pub mod worker;

pub use provider::{Provider, ProviderRegistry};
pub use types::*;

// ---------------------------------------------------------------------------
// String utilities
// ---------------------------------------------------------------------------

/// Truncate `s` to at most `max_bytes` bytes, rounding down to a char boundary.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate `s` to at most `max_bytes` bytes from the *end*, rounding up to a
/// char boundary.
pub(crate) fn truncate_str_end(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no provider registered for api: {0}")]
    NoProvider(String),
    #[error("no API key for provider: {0}")]
    NoApiKey(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("HTTP {status}: {message}")]
    HttpStatus {
        status: u16,
        message: String,
        /// Retry-After header value in seconds, if present.
        retry_after: Option<u64>,
    },
    #[error("parse error: {0}")]
    Parse(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("channel closed")]
    ChannelClosed,
    #[error("cancelled")]
    Cancelled,
    #[error("provider throttled until {0}")]
    Throttled(String),
}

pub type Result<T> = std::result::Result<T, Error>;
