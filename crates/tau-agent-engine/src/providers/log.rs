//! Log provider — a no-op LLM that returns an immediate end-turn response.
//!
//! Sessions using this provider can only be driven via `ExecuteTool`.
//! The agent loop exits cleanly after any tool results because the provider
//! returns a minimal `AssistantMessage` with `StopReason::Stop` (no text,
//! no tool calls).

use async_trait::async_trait;

use crate::provider::{EventReceiver, Provider};
use tau_agent_base::types::*;

const API_ID: &str = "log";

/// No-op provider that returns an immediate end-turn with empty content.
pub struct LogProvider;

#[async_trait]
impl Provider for LogProvider {
    fn api_id(&self) -> &str {
        API_ID
    }

    fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> tau_agent_base::Result<EventReceiver> {
        let (tx, rx) = smol::channel::unbounded();

        let output = AssistantMessage::empty(API_ID, "log", "log");

        std::thread::spawn(move || {
            tx.send_blocking(StreamEvent::Start {
                partial: output.clone(),
            })
            .ok();
            tx.send_blocking(StreamEvent::Done {
                reason: StopReason::Stop,
                message: output,
            })
            .ok();
        });

        Ok(rx)
    }
}

/// Create the built-in "log" model.
pub fn log_model() -> Model {
    Model {
        id: "log".into(),
        name: "Log (no LLM)".into(),
        api: API_ID.into(),
        provider: "log".into(),
        base_url: String::new(),
        thinking: ThinkingStyle::None,
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 1_000_000,
        max_tokens: 0,
        headers: Default::default(),
    }
}
