//! Shared types, wire protocol, and utilities for the tau agent workspace.
//!
//! This is the leaf crate that every other tau workspace crate depends on.
//! It has no async runtime deps — only serde, serde_json, and thiserror.

pub mod model_resolve;
pub mod paths;
pub mod plugin_protocol;
pub mod protocol;
pub mod subscription_usage;
pub mod tool_prompt;
pub mod types;

pub use types::*;

// ---------------------------------------------------------------------------
// JSON-line I/O helpers (sync only — async helpers live in dependent crates)
// ---------------------------------------------------------------------------

/// Serialize `val` as a single JSON line and flush the writer (sync).
pub fn write_json_line<T: serde::Serialize>(
    writer: &mut impl std::io::Write,
    val: &T,
) -> Result<()> {
    let mut line = serde_json::to_string(val).map_err(|e| Error::Io(e.to_string()))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|e| Error::Io(e.to_string()))?;
    writer.flush().map_err(|e| Error::Io(e.to_string()))?;
    Ok(())
}

/// Read a single JSON line (sync).  Returns `Ok(None)` on EOF.
pub fn read_json_line<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::BufRead,
) -> Result<Option<T>> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| Error::Io(e.to_string()))?;
    if n == 0 {
        return Ok(None);
    }
    let val = serde_json::from_str(&line).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(Some(val))
}

// ---------------------------------------------------------------------------
// Async JSON-line I/O helpers
// ---------------------------------------------------------------------------

/// Serialize `val` as a single JSON line and flush the writer (async).
pub async fn write_json_line_async<T: serde::Serialize>(
    writer: &mut (impl futures::io::AsyncWrite + Unpin),
    val: &T,
) -> Result<()> {
    use futures::io::AsyncWriteExt;
    let mut line = serde_json::to_string(val).map_err(|e| Error::Io(e.to_string()))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    writer.flush().await.map_err(|e| Error::Io(e.to_string()))?;
    Ok(())
}

/// Read a single JSON line (async).  Returns `Ok(None)` on EOF.
pub async fn read_json_line_async<T: serde::de::DeserializeOwned>(
    reader: &mut (impl futures::io::AsyncBufRead + Unpin),
) -> Result<Option<T>> {
    use futures::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    if n == 0 {
        return Ok(None);
    }
    let val = serde_json::from_str(&line).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(Some(val))
}

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
pub fn truncate_str_end(s: &str, max_bytes: usize) -> &str {
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
