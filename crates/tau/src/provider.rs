use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::types::{Context, Model, StreamEvent, StreamOptions};

/// Receiver end of a stream of events from an LLM provider.
pub type EventReceiver = smol::channel::Receiver<StreamEvent>;
/// Sender end — used by provider implementations.
pub type EventSender = smol::channel::Sender<StreamEvent>;

/// Trait implemented by each LLM API provider (Anthropic, OpenAI, …).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identifier for this API, e.g. `"anthropic-messages"`.
    fn api_id(&self) -> &str;

    /// Start streaming a completion. Returns immediately with a channel receiver.
    /// Events (including errors) are delivered through the channel.
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> crate::Result<EventReceiver>;
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

    /// Stream a completion using the provider registered for `model.api`.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> crate::Result<EventReceiver> {
        let provider = self
            .get(&model.api)
            .ok_or_else(|| crate::Error::NoProvider(model.api.clone()))?;
        provider.stream(model, context, options)
    }
}
