//! Mock LLM provider for testing.
//!
//! Returns pre-configured responses from a queue. Each call to `stream()`
//! pops the next response and sends it as proper stream events.
//!
//! Also provides `MockToolExecutor` for testing tool execution flows.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::provider::{EventReceiver, EventSender, Provider};
use crate::types::*;
use crate::worker::ToolExecutor;

const API_ID: &str = "mock";

// ===========================================================================
// MockResponse
// ===========================================================================

/// A pre-configured response for the mock provider.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Return assistant text.
    Text(String),
    /// Return tool calls (then expect tool results before next call).
    ToolCalls(Vec<ToolCall>),
    /// Return an error message.
    Error(String),
    /// Return partial text then error (no TextEnd/Done — simulates mid-stream failure).
    PartialText { text: String, error: String },
    /// Wait `delay_ms` then deliver the inner response.
    Delayed {
        delay_ms: u64,
        response: Box<MockResponse>,
    },
}

// ===========================================================================
// MockCapture — records what the provider saw on each turn
// ===========================================================================

#[derive(Clone)]
pub struct MockCapture {
    /// Zero-based turn index (order of `stream()` calls).
    pub index: usize,
    /// The context that was passed to `stream()`.
    pub context: Context,
    /// When this turn started.
    pub timestamp: std::time::Instant,
}

// ===========================================================================
// MockProvider (Arc/Handle pattern)
// ===========================================================================

struct MockProviderInner {
    responses: Mutex<VecDeque<MockResponse>>,
    captures: Mutex<Vec<MockCapture>>,
}

/// Mock provider that returns pre-configured responses.
pub struct MockProvider {
    inner: Arc<MockProviderInner>,
}

/// Cheap cloneable handle to inspect mock state after the test.
#[derive(Clone)]
pub struct MockProviderHandle {
    inner: Arc<MockProviderInner>,
}

impl MockProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            inner: Arc::new(MockProviderInner {
                responses: Mutex::new(responses.into()),
                captures: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Obtain a handle that can be used to inspect captures after the provider
    /// has been moved into the agent.
    pub fn handle(&self) -> MockProviderHandle {
        MockProviderHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl MockProviderHandle {
    /// Return a snapshot of all captured contexts (one per `stream()` call).
    pub fn captures(&self) -> Vec<MockCapture> {
        self.inner.captures.lock().unwrap().clone()
    }

    /// Convenience: return the wall-clock duration of each turn pair (i→i+1).
    pub fn turn_durations(&self) -> Vec<std::time::Duration> {
        let caps = self.inner.captures.lock().unwrap();
        caps.windows(2)
            .map(|w| w[1].timestamp.duration_since(w[0].timestamp))
            .collect()
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
        // Capture context before popping the response.
        {
            let mut caps = self.inner.captures.lock().unwrap();
            let index = caps.len();
            caps.push(MockCapture {
                index,
                context: _context.clone(),
                timestamp: std::time::Instant::now(),
            });
        }

        let (tx, rx) = smol::channel::unbounded();

        let response = self
            .inner
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

// ===========================================================================
// Stream event generation
// ===========================================================================

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
        MockResponse::PartialText { text, error } => {
            // Emit TextStart + TextDelta, then an Error — no TextEnd/Done.
            output.content.push(AssistantContent::Text(TextContent {
                text: text.clone(),
                text_signature: None,
            }));

            tx.send_blocking(StreamEvent::TextStart {
                content_index: 0,
                partial: output.clone(),
            })
            .ok();
            tx.send_blocking(StreamEvent::TextDelta {
                content_index: 0,
                delta: text,
                partial: output.clone(),
            })
            .ok();

            output.stop_reason = StopReason::Error;
            output.error_message = Some(error);
            tx.send_blocking(StreamEvent::Error {
                reason: StopReason::Error,
                error: output,
            })
            .ok();
        }
        MockResponse::Delayed { delay_ms, response } => {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            // Recurse — the Start event was already sent, but the inner call
            // will send its own Start. That's intentional for simplicity; the
            // consumer should tolerate duplicate Start events.
            send_mock_response(tx, *response);
        }
    }
}

// ===========================================================================
// MockToolExecutor
// ===========================================================================

/// Pre-configured response for a mock tool.
#[derive(Debug, Clone)]
pub enum MockToolResponse {
    /// Successful tool result text.
    Success(String),
    /// Tool-level error (is_error = true in the result).
    ToolError(String),
    /// Executor-level error (returns Err from execute).
    ExecutorError(String),
    /// Wait then deliver the inner response.
    Delayed {
        delay_ms: u64,
        response: Box<MockToolResponse>,
    },
}

/// Captures a tool call that went through the mock executor.
#[derive(Clone, Debug)]
pub struct MockToolCapture {
    pub tool_call: ToolCall,
    pub timestamp: std::time::Instant,
}

struct MockToolExecutorInner {
    /// Per-tool-name response queues.
    responses: Mutex<HashMap<String, VecDeque<MockToolResponse>>>,
    /// Fallback when a tool has no queued response.
    default: Mutex<Option<MockToolResponse>>,
    captures: Mutex<Vec<MockToolCapture>>,
}

/// Mock tool executor — can be moved into the agent.
pub struct MockToolExecutor {
    inner: Arc<MockToolExecutorInner>,
}

/// Cheap handle for setting up / inspecting the mock after it's been moved.
#[derive(Clone)]
pub struct MockToolExecutorHandle {
    inner: Arc<MockToolExecutorInner>,
}

impl MockToolExecutor {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MockToolExecutorInner {
                responses: Mutex::new(HashMap::new()),
                default: Mutex::new(None),
                captures: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Obtain a handle for setup & inspection.
    pub fn handle(&self) -> MockToolExecutorHandle {
        MockToolExecutorHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl MockToolExecutorHandle {
    /// Queue a response for a specific tool name.
    pub fn on_tool(&self, name: &str, response: MockToolResponse) {
        self.inner
            .responses
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push_back(response);
    }

    /// Set the default response for tools with no queued response.
    pub fn set_default(&self, response: MockToolResponse) {
        *self.inner.default.lock().unwrap() = Some(response);
    }

    /// Return a snapshot of all captured tool calls.
    pub fn captures(&self) -> Vec<MockToolCapture> {
        self.inner.captures.lock().unwrap().clone()
    }

    /// Create a new `MockToolExecutor` sharing the same inner state.
    pub fn executor(&self) -> MockToolExecutor {
        MockToolExecutor {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Recursion-safe helper: resolve a `MockToolResponse` to a `ToolResultMessage`.
fn execute_mock_tool_response(
    tool_call: &ToolCall,
    resp: MockToolResponse,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<ToolResultMessage>> + Send + '_>> {
    Box::pin(async move {
        match resp {
            MockToolResponse::Success(text) => Ok(ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: vec![ToolResultContent::Text(TextContent {
                    text,
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: timestamp_ms(),
            }),
            MockToolResponse::ToolError(msg) => Ok(ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: msg,
                    text_signature: None,
                })],
                details: None,
                is_error: true,
                timestamp: timestamp_ms(),
            }),
            MockToolResponse::ExecutorError(msg) => Err(crate::Error::Http(msg)),
            MockToolResponse::Delayed { delay_ms, response } => {
                smol::Timer::after(std::time::Duration::from_millis(delay_ms)).await;
                execute_mock_tool_response(tool_call, *response).await
            }
        }
    })
}

#[async_trait]
impl ToolExecutor for MockToolExecutor {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        _output_tx: &smol::channel::Sender<String>,
    ) -> crate::Result<ToolResultMessage> {
        // Capture.
        self.inner.captures.lock().unwrap().push(MockToolCapture {
            tool_call: tool_call.clone(),
            timestamp: std::time::Instant::now(),
        });

        // Pop queued response, or use default, or error.
        let resp = {
            let mut map = self.inner.responses.lock().unwrap();
            if let Some(queue) = map.get_mut(&tool_call.name) {
                queue.pop_front()
            } else {
                None
            }
        }
        .or_else(|| self.inner.default.lock().unwrap().clone())
        .unwrap_or(MockToolResponse::ExecutorError(format!(
            "no mock response for tool '{}'",
            tool_call.name
        )));

        execute_mock_tool_response(tool_call, resp).await
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Create a minimal `Tool` schema for use in tests.
pub fn mock_tool(name: &str, description: &str) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
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
