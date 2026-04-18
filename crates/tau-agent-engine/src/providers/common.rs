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
/// Default timeout for receiving response headers (time-to-first-byte).
///
/// Modest bump above ureq's default — non-thinking models normally reply
/// within seconds; this covers occasional slow turns without waiting forever
/// on a genuinely hung provider.
pub const TIMEOUT_RECV_RESPONSE: Duration = Duration::from_secs(180);
/// First-byte timeout for adaptive-thinking-capable models with thinking
/// turned on. Opus 4.7 and similar can spend several minutes reasoning
/// before emitting any SSE event, so we need a much larger budget here.
pub const TIMEOUT_RECV_RESPONSE_ADAPTIVE: Duration = Duration::from_secs(600);

/// Pick the time-to-first-byte timeout for a given model + options.
///
/// Adaptive-thinking Anthropic models with `thinking_enabled` (either
/// explicitly `Some(true)` or inferred from effort/budget) can sit silent
/// for minutes while reasoning; they get [`TIMEOUT_RECV_RESPONSE_ADAPTIVE`].
/// Everything else uses the default [`TIMEOUT_RECV_RESPONSE`].
pub fn recv_timeout_for(model: &Model, options: &StreamOptions) -> Duration {
    if is_adaptive_thinking_call(model, options) {
        TIMEOUT_RECV_RESPONSE_ADAPTIVE
    } else {
        TIMEOUT_RECV_RESPONSE
    }
}

/// True when this call is an adaptive-thinking Anthropic request: the model
/// has `ThinkingStyle::Anthropic`, supports adaptive thinking, and thinking
/// is on (either explicit `thinking_enabled == Some(true)`, or inferred from
/// the presence of a budget/effort with `thinking_enabled != Some(false)`).
fn is_adaptive_thinking_call(model: &Model, options: &StreamOptions) -> bool {
    if model.thinking != ThinkingStyle::Anthropic {
        return false;
    }
    if !crate::providers::anthropic::supports_adaptive_thinking(&model.id) {
        return false;
    }
    match options.thinking_enabled {
        Some(true) => true,
        Some(false) => false,
        None => options.thinking_budget.is_some() || options.thinking_effort.is_some(),
    }
}

/// Common context carried into the streaming thread.
pub(crate) struct StreamCtx<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub api_id: &'a str,
    pub provider_name: &'a str,
    pub model_id: &'a str,
    pub model: &'a Model,
    /// Time-to-first-byte timeout. Providers apply this via ureq's
    /// `timeout_recv_response`. Computed at call time via
    /// [`recv_timeout_for`] so thinking-capable models get a larger budget.
    pub recv_response_timeout: Duration,
}

/// Send a [`StreamEvent`] over the channel, mapping send errors to
/// [`tau_agent_base::Error::ChannelClosed`].
pub(crate) fn send_event(tx: &EventSender, event: StreamEvent) -> tau_agent_base::Result<()> {
    tx.send_blocking(event)
        .map_err(|_| tau_agent_base::Error::ChannelClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mk_model(id: &str, style: ThinkingStyle) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://example.invalid".to_string(),
            thinking: style,
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8_192,
            headers: HashMap::new(),
        }
    }

    #[test]
    fn recv_timeout_uses_adaptive_for_thinking_on_adaptive_model() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        assert_eq!(
            recv_timeout_for(&model, &options),
            TIMEOUT_RECV_RESPONSE_ADAPTIVE
        );
    }

    #[test]
    fn recv_timeout_uses_adaptive_when_inferred_from_effort() {
        let model = mk_model("claude-sonnet-4.6", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_effort: Some(ThinkingEffort::High),
            ..StreamOptions::default()
        };
        // thinking_enabled is None but effort is set -> adaptive path enabled.
        assert_eq!(
            recv_timeout_for(&model, &options),
            TIMEOUT_RECV_RESPONSE_ADAPTIVE
        );
    }

    #[test]
    fn recv_timeout_default_when_thinking_disabled_explicitly() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(false),
            thinking_effort: Some(ThinkingEffort::High),
            ..StreamOptions::default()
        };
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_default_for_non_adaptive_anthropic_model() {
        let model = mk_model("claude-sonnet-3-5", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_default_for_non_anthropic_style() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::None);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        // Even on an adaptive-capable id, ThinkingStyle::None disqualifies.
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_default_when_no_thinking_signal() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions::default();
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }
}
