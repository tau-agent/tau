//! Cumulative usage tracking shared by the tau agent frontends.
//!
//! Both `tau-agent` (CLI) and `tau-agent-tui` accumulate per-session token
//! counts, cost, and context-window usage via this struct. The struct itself
//! is intentionally passive: frontends call [`UsageTotals::add`] on every
//! assistant response to fold in the latest [`Usage`] delta, and render the
//! totals however they like (a plain `eprintln!` line for the CLI, a status
//! bar for the TUI).
//!
//! `context_window` and `is_subscription` are set by the frontend once at
//! session init (from the resolved model / auth credential) — they're not
//! updated by `add`.
//!
//! See catalog item #5 in task #573 for the dedup rationale.

use crate::types::Usage;

/// Running totals across all responses in a session.
///
/// Fields use `u64` for tokens (always non-negative, fit comfortably) and
/// `f64` for cost (dollars). `context_tokens` is `Option<u64>` because it's
/// only populated after the first successful response.
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// Context window size from the model. Populated once at session init;
    /// `0` means "unknown / don't render a context estimate".
    pub context_window: u64,
    /// Context tokens from the last successful response: the total input
    /// (fresh + cached) the provider saw on that turn. `None` until the first
    /// response lands.
    pub context_tokens: Option<u64>,
    /// Whether the session is using an OAuth subscription (cost is fully
    /// subsidised). Frontends may surface this differently in cost display.
    pub is_subscription: bool,
}

impl UsageTotals {
    /// Fold a per-response [`Usage`] delta into the running totals.
    ///
    /// Token counters accumulate; cost accumulates (`cost.total`);
    /// `context_tokens` is *replaced* with the most recent response's total
    /// input (fresh + cache_read + cache_write), giving a running estimate of
    /// the conversation's current context size.
    pub fn add(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
        // Context estimate: last response's total input (fresh + cached).
        self.context_tokens = Some(usage.input + usage.cache_read + usage.cache_write);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Cost;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
            },
        }
    }

    #[test]
    fn add_accumulates_tokens_and_cost() {
        let mut t = UsageTotals::default();
        t.add(&usage(10, 5, 2, 1, 0.01));
        t.add(&usage(20, 7, 3, 0, 0.02));

        assert_eq!(t.input, 30);
        assert_eq!(t.output, 12);
        assert_eq!(t.cache_read, 5);
        assert_eq!(t.cache_write, 1);
        assert!((t.cost - 0.03).abs() < 1e-9);
    }

    #[test]
    fn add_replaces_context_tokens_with_last_response() {
        let mut t = UsageTotals::default();
        t.add(&usage(10, 5, 2, 1, 0.0));
        // After first call: 10 + 2 + 1 = 13.
        assert_eq!(t.context_tokens, Some(13));

        t.add(&usage(50, 5, 4, 0, 0.0));
        // After second call: last response's 50 + 4 + 0 = 54 — replaces, not adds.
        assert_eq!(t.context_tokens, Some(54));
    }

    #[test]
    fn default_has_no_context_tokens() {
        let t = UsageTotals::default();
        assert_eq!(t.context_tokens, None);
        assert_eq!(t.input, 0);
        assert_eq!(t.cost, 0.0);
        assert!(!t.is_subscription);
    }
}
