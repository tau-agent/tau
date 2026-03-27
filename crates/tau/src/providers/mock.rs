//! Mock LLM provider for testing.
//!
//! Returns pre-configured responses from a queue. Each call to `stream()`
//! pops the next response and sends it as proper stream events.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::provider::{EventReceiver, EventSender, Provider};
use crate::types::*;

const API_ID: &str = "mock";

/// A pre-configured response for the mock provider.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Return assistant text.
    Text(String),
    /// Return tool calls (then expect tool results before next call).
    ToolCalls(Vec<ToolCall>),
    /// Return an error message.
    Error(String),
}

/// Mock provider that returns pre-configured responses.
pub struct MockProvider {
    responses: Mutex<VecDeque<MockResponse>>,
}

impl MockProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn api_id(&self) -> &str {
        API_ID
    }

    fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> crate::Result<EventReceiver> {
        let (tx, rx) = smol::channel::unbounded();

        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(MockResponse::Error("no more mock responses".into()));

        std::thread::spawn(move || {
            send_mock_response(&tx, response);
        });

        Ok(rx)
    }
}

fn send_mock_response(tx: &EventSender, response: MockResponse) {
    let mut output = AssistantMessage::empty(API_ID, "mock", "mock-model");

    tx.send_blocking(StreamEvent::Start {
        partial: output.clone(),
    })
    .ok();

    match response {
        MockResponse::Text(text) => {
            output.content.push(AssistantContent::Text(TextContent {
                text: text.clone(),
                text_signature: None,
            }));
            output.usage.input = 100;
            output.usage.output = text.len() as u64 / 4;
            output.stop_reason = StopReason::Stop;

            tx.send_blocking(StreamEvent::TextStart {
                content_index: 0,
                partial: output.clone(),
            })
            .ok();
            tx.send_blocking(StreamEvent::TextDelta {
                content_index: 0,
                delta: text.clone(),
                partial: output.clone(),
            })
            .ok();
            tx.send_blocking(StreamEvent::TextEnd {
                content_index: 0,
                content: text,
                partial: output.clone(),
            })
            .ok();
            tx.send_blocking(StreamEvent::Done {
                reason: StopReason::Stop,
                message: output,
            })
            .ok();
        }
        MockResponse::ToolCalls(calls) => {
            for (i, tc) in calls.iter().enumerate() {
                output.content.push(AssistantContent::ToolCall(tc.clone()));
                tx.send_blocking(StreamEvent::ToolcallStart {
                    content_index: i,
                    partial: output.clone(),
                })
                .ok();
                tx.send_blocking(StreamEvent::ToolcallEnd {
                    content_index: i,
                    tool_call: tc.clone(),
                    partial: output.clone(),
                })
                .ok();
            }
            output.usage.input = 100;
            output.usage.output = 50;
            output.stop_reason = StopReason::ToolUse;

            tx.send_blocking(StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: output,
            })
            .ok();
        }
        MockResponse::Error(msg) => {
            output.stop_reason = StopReason::Error;
            output.error_message = Some(msg);
            tx.send_blocking(StreamEvent::Error {
                reason: StopReason::Error,
                error: output,
            })
            .ok();
        }
    }
}

/// Create a mock model for testing.
pub fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock Model".into(),
        api: API_ID.into(),
        provider: "mock".into(),
        base_url: "http://mock".into(),
        thinking: ThinkingStyle::None,
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 100_000,
        max_tokens: 4_096,
        headers: Default::default(),
    }
}
