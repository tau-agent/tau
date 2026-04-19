//! Subscription usage types and OAuth token detection.
//!
//! These are pure data types used by the wire protocol. The actual
//! OAuth flow and credential storage lives in `tau-agent-lib::auth`.

use serde::{Deserialize, Serialize};

/// Check if an API key is an OAuth token (starts with `sk-ant-oat`).
pub fn is_oauth_token(key: &str) -> bool {
    key.starts_with("sk-ant-oat")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageBucket {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtraUsage {
    #[serde(default)]
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubscriptionUsage {
    pub five_hour: Option<UsageBucket>,
    pub seven_day: Option<UsageBucket>,
    pub seven_day_sonnet: Option<UsageBucket>,
    pub seven_day_opus: Option<UsageBucket>,
    pub extra_usage: Option<ExtraUsage>,
}
