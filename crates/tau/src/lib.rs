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
pub mod server;
pub mod system_prompt;
pub mod throttle;
pub mod tools;
pub mod types;
pub mod worker;

pub use provider::{Provider, ProviderRegistry};
pub use types::*;

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
