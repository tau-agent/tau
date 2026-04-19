use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use tau_agent_base::types::{Context, Model, StreamEvent, StreamOptions};

/// Receiver end of a stream of events from an LLM provider.
pub type EventReceiver = smol::channel::Receiver<StreamEvent>;
/// Sender end — used by provider implementations.
pub type EventSender = smol::channel::Sender<StreamEvent>;

/// Trait implemented by each LLM API provider (Anthropic, OpenAI, …).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identifier for this API, e.g. `"anthropic-messages"`.
    fn api_id(&self) -> &str;

    /// Whether this provider requires an API key to run a turn.
    ///
    /// Real LLM providers return `true` (the default). No-op providers
    /// such as the built-in `log` provider — which never make an outbound
    /// API call — override this to `false` so the agent runner skips
    /// the `resolve_api_key` preflight check.
    fn needs_api_key(&self) -> bool {
        true
    }

    /// Start streaming a completion. Returns immediately with a channel receiver.
    /// Events (including errors) are delivered through the channel.
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> tau_agent_base::Result<EventReceiver>;
}

/// Simple registry mapping API id → provider.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: impl Provider + 'static) {
        let api_id = provider.api_id().to_string();
        self.providers.insert(api_id, Arc::new(provider));
    }

    pub fn get(&self, api: &str) -> Option<&dyn Provider> {
        self.providers.get(api).map(|p| p.as_ref())
    }

    /// Convenience: whether the provider registered for `api` needs an API key.
    ///
    /// Returns `true` if the api isn't registered (conservative default —
    /// the caller will fail loudly downstream with `NoProvider`).
    pub fn needs_api_key(&self, api: &str) -> bool {
        self.get(api).map(|p| p.needs_api_key()).unwrap_or(true)
    }

    /// Stream a completion using the provider registered for `model.api`.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> tau_agent_base::Result<EventReceiver> {
        let provider = self
            .get(&model.api)
            .ok_or_else(|| tau_agent_base::Error::NoProvider(model.api.clone()))?;
        provider.stream(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::log::LogProvider;

    #[test]
    fn log_provider_does_not_need_api_key() {
        assert!(!LogProvider.needs_api_key());
    }

    #[test]
    fn registry_needs_api_key_defaults_true_for_unknown_api() {
        let reg = ProviderRegistry::new();
        assert!(
            reg.needs_api_key("definitely-not-registered"),
            "unknown api must be treated as key-required"
        );
    }

    #[test]
    fn registry_needs_api_key_reports_false_for_log() {
        let mut reg = ProviderRegistry::new();
        reg.register(LogProvider);
        assert!(!reg.needs_api_key("log"));
    }
}
