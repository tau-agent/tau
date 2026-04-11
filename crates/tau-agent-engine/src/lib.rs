//! Core agent loop and LLM providers for the tau agent.
//!
//! This crate provides the streaming agent loop, context compaction,
//! provider trait and implementations (Anthropic, OpenAI), and the
//! system prompt builder. Usable as a standalone library without the daemon.

pub mod agent;
pub mod compaction;
pub mod provider;
pub mod providers;
pub mod system_prompt;
pub mod throttle;

pub use provider::{Provider, ProviderRegistry};
