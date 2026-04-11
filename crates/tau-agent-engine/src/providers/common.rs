//! Shared utilities for provider implementations.

use std::time::Duration;

use crate::provider::EventSender;
use tau_agent_base::types::*;

/// Timeout for TCP + TLS connection establishment.
pub const TIMEOUT_CONNECT: Duration = Duration::from_secs(30);
/// Timeout for sending the request headers.
pub const TIMEOUT_SEND_REQUEST: Duration = Duration::from_secs(30);
/// Timeout for sending the request body (JSON payload).
pub const TIMEOUT_SEND_BODY: Duration = Duration::from_secs(30);
/// Timeout for receiving response headers (time-to-first-byte).
pub const TIMEOUT_RECV_RESPONSE: Duration = Duration::from_secs(120);

/// Common context carried into the streaming thread.
pub(crate) struct StreamCtx<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub api_id: &'a str,
    pub provider_name: &'a str,
    pub model_id: &'a str,
    pub model: &'a Model,
}

/// Send a [`StreamEvent`] over the channel, mapping send errors to
/// [`tau_agent_base::Error::ChannelClosed`].
pub(crate) fn send_event(tx: &EventSender, event: StreamEvent) -> tau_agent_base::Result<()> {
    tx.send_blocking(event)
        .map_err(|_| tau_agent_base::Error::ChannelClosed)
}
