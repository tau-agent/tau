//! Plugin SDK for the tau agent.
//!
//! This crate provides everything a plugin author needs in one place:
//! - The `ToolExecutor` trait (the tool execution abstraction)
//! - The `tunnel` module for plugin ↔ server communication
//! - Re-exports of all plugin-relevant types from `tau-agent-base`

pub mod executor;
pub mod tunnel;

// Re-export the ToolExecutor trait at crate root for convenience
pub use executor::ToolExecutor;

// Re-export plugin-relevant types from tau-agent-base
pub use tau_agent_base::paths::data_dir;
pub use tau_agent_base::plugin_protocol::*;
pub use tau_agent_base::protocol::{Request, Response, SessionInfo};
pub use tau_agent_base::tool_prompt::ToolPrompt;
pub use tau_agent_base::types::*;
pub use tau_agent_base::{Error, Result, read_json_line, write_json_line};
