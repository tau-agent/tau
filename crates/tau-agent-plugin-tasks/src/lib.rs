//! Task system plugin for the tau agent.
//!
//! This crate implements the built-in tasks plugin (`tau plugin-tasks`).
//! It has its own SQLite database, state machine, scheduler, merge queue,
//! and git helpers.
//!
//! The plugin communicates with the server via the plugin protocol over
//! stdin/stdout — it does NOT depend on `tau-agent-server`.

pub mod tasks;
pub mod tasks_config;
pub mod tasks_db;
pub mod tasks_git;
pub mod tasks_merge;
pub mod tasks_scheduler;

/// Entry point for the `tau plugin-tasks` subprocess.
pub fn run_tasks_plugin() {
    tasks::run_tasks_plugin();
}
