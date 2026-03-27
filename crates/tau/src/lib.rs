pub mod agent;
pub mod auth;
pub mod client;
pub mod compaction;
pub mod config;
pub mod db;
pub mod protocol;
pub mod provider;
pub mod providers;
pub mod server;
pub mod system_prompt;
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
    #[error("parse error: {0}")]
    Parse(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("channel closed")]
    ChannelClosed,
}

pub type Result<T> = std::result::Result<T, Error>;
