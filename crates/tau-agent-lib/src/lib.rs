// Server-side modules (still live here until full server extraction)
pub mod auth;
pub mod config;
pub mod db;
pub mod logging;
pub mod migration;
pub mod models_config;
pub mod plugin;
pub mod replay;
pub mod server;
pub mod shutdown;
pub mod worker;

// Re-export from tau-agent-base for backward compatibility
pub use tau_agent_base::config_chain;
pub use tau_agent_base::model_resolve;
pub use tau_agent_base::paths;
pub use tau_agent_base::plugin_protocol;
pub use tau_agent_base::project;
pub use tau_agent_base::protocol;
pub use tau_agent_base::subscription_usage;
pub use tau_agent_base::tool_prompt;
pub use tau_agent_base::types;
pub use tau_agent_base::usage_totals;

pub use tau_agent_base::{
    Error, Result, read_json_line, read_json_line_async, truncate_str, truncate_str_end,
    write_json_line, write_json_line_async,
};

pub use types::*;

// Re-export from tau-agent-engine for backward compatibility
pub use tau_agent_engine::agent;
pub use tau_agent_engine::compaction;
pub use tau_agent_engine::provider;
pub use tau_agent_engine::providers;
pub use tau_agent_engine::system_prompt;
pub use tau_agent_engine::throttle;
pub use tau_agent_engine::{Provider, ProviderRegistry};

// Re-export from tau-agent-plugin-tasks for backward compatibility
pub use tau_agent_plugin_tasks::tasks;
pub use tau_agent_plugin_tasks::tasks_config;
pub use tau_agent_plugin_tasks::tasks_db;
pub use tau_agent_plugin_tasks::tasks_git;
pub use tau_agent_plugin_tasks::tasks_merge;
pub use tau_agent_plugin_tasks::tasks_scheduler;
pub use tau_agent_plugin_tasks::tasks_state;

// Re-export from tau-agent-client for backward compatibility
pub use tau_agent_client as client;

// Re-export from tau-agent-plugin-worker for backward compatibility
pub use tau_agent_plugin_worker::orchestration;
pub use tau_agent_plugin_worker::tools;

/// Process-global env-var mutex for tests that mutate `XDG_CONFIG_HOME` / `HOME`.
///
/// Multiple test modules (`models_config`, `plugin`, …) mutate environment
/// variables.  Since cargo runs tests in parallel within a single binary,
/// all such tests must acquire this mutex first.
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
