//! Tool execution abstraction.
//!
//! The `ToolExecutor` trait is the contract between "something that runs tools"
//! and "something that consumes tool results". Plugin authors don't implement it
//! directly (they speak the wire protocol), but consumers of plugin handles do
//! (`PluginExecutor` in server, `InProcessWorker` in worker).

use async_trait::async_trait;
use tau_agent_base::types::{ToolCall, ToolResultMessage};

/// Trait for tool execution (allows plugin-based or in-process).
#[async_trait]
pub trait ToolExecutor: Send {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        output_tx: &smol::channel::Sender<String>,
    ) -> tau_agent_base::Result<ToolResultMessage>;
}
