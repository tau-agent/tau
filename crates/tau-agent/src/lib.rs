pub mod agent;
pub mod auth;
pub mod client;
pub mod compaction;
pub mod config;
pub mod db;
pub mod models_config;
pub mod orchestration;
pub mod plugin;
pub mod provider;
pub mod providers;
pub mod replay;
pub mod server;
pub mod system_prompt;
pub mod throttle;
pub mod tools;
pub mod worker;

// Re-export from tau-agent-base for backward compatibility
pub use tau_agent_base::model_resolve;
pub use tau_agent_base::paths;
pub use tau_agent_base::plugin_protocol;
pub use tau_agent_base::protocol;
pub use tau_agent_base::subscription_usage;
pub use tau_agent_base::tool_prompt;
pub use tau_agent_base::types;

pub use tau_agent_base::{
    Error, Result, read_json_line, read_json_line_async, truncate_str, truncate_str_end,
    write_json_line, write_json_line_async,
};

pub use provider::{Provider, ProviderRegistry};
pub use types::*;

// Re-export from tau-agent-plugin-tasks for backward compatibility
pub use tau_agent_plugin_tasks::tasks;
pub use tau_agent_plugin_tasks::tasks_config;
pub use tau_agent_plugin_tasks::tasks_db;
pub use tau_agent_plugin_tasks::tasks_git;
pub use tau_agent_plugin_tasks::tasks_merge;
pub use tau_agent_plugin_tasks::tasks_scheduler;
