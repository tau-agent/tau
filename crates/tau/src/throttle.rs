//! Global per-provider request throttling.
//!
//! When a provider returns a rate limit error indicating the account's usage
//! bucket is exhausted (e.g. Anthropic 5h/7d subscription limits), the
//! throttle blocks all requests to that provider until the limit resets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared throttle state for all providers.
#[derive(Clone, Default)]
pub struct ProviderThrottle {
    inner: Arc<Mutex<ThrottleState>>,
}

#[derive(Default)]
struct ThrottleState {
    /// Provider name → sleep-until instant.
    blocked_until: HashMap<String, Instant>,
}

impl ProviderThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a provider is currently throttled.
    /// Returns `Some(remaining_duration)` if blocked, `None` if clear.
    pub fn check(&self, provider: &str) -> Option<Duration> {
        let state = self.inner.lock().unwrap();
        if let Some(until) = state.blocked_until.get(provider) {
            let now = Instant::now();
            if now < *until {
                return Some(*until - now);
            }
        }
        None
    }

    /// Block a provider for `duration` from now.
    pub fn block_for(&self, provider: &str, duration: Duration) {
        let mut state = self.inner.lock().unwrap();
        let until = Instant::now() + duration;
        // Only extend, never shorten an existing block
        let entry = state
            .blocked_until
            .entry(provider.to_string())
            .or_insert(Instant::now());
        if until > *entry {
            *entry = until;
        }
    }

    /// Block a provider until a specific number of seconds from now.
    pub fn block_for_secs(&self, provider: &str, secs: u64) {
        self.block_for(provider, Duration::from_secs(secs));
    }

    /// Clear throttle for a provider.
    pub fn clear(&self, provider: &str) {
        let mut state = self.inner.lock().unwrap();
        state.blocked_until.remove(provider);
    }

    /// Process an error message and apply throttling if it indicates a rate limit.
    /// Returns true if the error was a rate limit that should be retried after sleeping.
    pub fn handle_error(&self, provider: &str, err_msg: &str, retry_after: Option<u64>) -> bool {
        let lower = err_msg.to_lowercase();

        // Anthropic subscription rate limits
        if lower.contains("rate limit") || lower.contains("429") {
            // Use retry-after if available, otherwise default to 60s
            let secs = retry_after.unwrap_or(60);
            eprintln!(
                "provider '{}' rate limited, blocking for {}s",
                provider, secs
            );
            self.block_for_secs(provider, secs);
            return true;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_throttle_by_default() {
        let t = ProviderThrottle::new();
        assert!(t.check("anthropic").is_none());
    }

    #[test]
    fn test_block_and_check() {
        let t = ProviderThrottle::new();
        t.block_for_secs("anthropic", 10);
        let remaining = t.check("anthropic").unwrap();
        assert!(remaining.as_secs() >= 9);
        assert!(t.check("openai").is_none());
    }

    #[test]
    fn test_clear() {
        let t = ProviderThrottle::new();
        t.block_for_secs("anthropic", 10);
        assert!(t.check("anthropic").is_some());
        t.clear("anthropic");
        assert!(t.check("anthropic").is_none());
    }

    #[test]
    fn test_handle_rate_limit_error() {
        let t = ProviderThrottle::new();
        let handled = t.handle_error("anthropic", "HTTP 429: rate limit exceeded", Some(30));
        assert!(handled);
        let remaining = t.check("anthropic").unwrap();
        assert!(remaining.as_secs() >= 29);
    }

    #[test]
    fn test_handle_non_rate_limit_error() {
        let t = ProviderThrottle::new();
        let handled = t.handle_error("anthropic", "HTTP 500: internal server error", None);
        assert!(!handled);
        assert!(t.check("anthropic").is_none());
    }

    #[test]
    fn test_block_only_extends() {
        let t = ProviderThrottle::new();
        t.block_for_secs("anthropic", 100);
        let r1 = t.check("anthropic").unwrap();
        t.block_for_secs("anthropic", 10); // shorter — should NOT reduce
        let r2 = t.check("anthropic").unwrap();
        assert!(r2.as_secs() >= r1.as_secs() - 1); // allow 1s tolerance
    }
}
