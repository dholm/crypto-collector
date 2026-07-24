//! Worker retry & backpressure classification (SPEC-SCHED-002 REQ-SCHED-060..063).
//!
//! Shared between the collection-queue and backfill workers. Holds the classified
//! dispatch-error type ([`DispatchError`], REQ-SCHED-063) and the pure retry-bound
//! helper ([`max_claims_in_window`], REQ-SCHED-062) that documents the claim-count
//! ceiling the inter-cycle pause enforces during a provider cooldown.

use thiserror::Error;

/// Classification of a dispatch / chunk-processing failure in the collector workers
/// (REQ-SCHED-063.1).
///
/// The distinction drives the worker's failure handling:
/// - [`DispatchError::Permanent`] — terminal (coin not found, no provider supports the
///   required capability, unknown dispatch kind). The worker fails the row immediately
///   on the first attempt; it neither consumes nor relies on the `max_attempts` retry
///   budget (REQ-SCHED-063.2, self-consistent with REQ-SCHED-060.3).
/// - [`DispatchError::Transient`] — retryable (provider network / rate-limit error, DB
///   upsert error, pacer misconfiguration). The worker follows the retry-with-backoff
///   path (REQ-SCHED-063.3).
///
/// Backpressure (pacer `Cooldown` / `CreditExhausted`) is NOT modelled here — it is a
/// non-failure soft-skip handled on the `Ok` channel of the dispatch functions
/// (REQ-SCHED-061), so it never consumes the retry budget.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A retryable failure: retry with backoff up to `max_attempts` (REQ-SCHED-063.3).
    #[error("transient: {0}")]
    Transient(String),

    /// A terminal failure: fail immediately on the first attempt, without consuming or
    /// relying on the retry budget (REQ-SCHED-063.2).
    #[error("permanent: {0}")]
    Permanent(String),
}

impl DispatchError {
    /// True for a terminal (permanent) failure.
    pub fn is_permanent(&self) -> bool {
        matches!(self, DispatchError::Permanent(_))
    }
}

/// Upper bound on how many claims a worker can issue while a provider is in a cooldown
/// of `window_secs`, given an inter-cycle pause of `pause_secs` (REQ-SCHED-062.2).
///
/// Returns `⌈window_secs / pause_secs⌉ + 1` — the `+1` covers the claim that discovers
/// the cooldown at the very start of the window, before the first pause. A `pause_secs`
/// of `0` is clamped to `1` (a zero pause would be the busy-loop this bound exists to
/// prevent). This is the ceiling the soft-skip pause enforces: each cooldown cycle
/// costs exactly one claim + one bounded pause, never a tight loop against the DB.
pub fn max_claims_in_window(window_secs: i64, pause_secs: i64) -> u64 {
    let window = window_secs.max(0);
    let pause = pause_secs.max(1);
    // Ceiling division ((window + pause - 1) / pause), then +1 for the initial
    // cooldown-discovering claim.
    (((window + pause - 1) / pause) as u64) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_is_permanent() {
        assert!(DispatchError::Permanent("coin x not found".into()).is_permanent());
    }

    #[test]
    fn transient_is_not_permanent() {
        assert!(!DispatchError::Transient("network blip".into()).is_permanent());
    }

    #[test]
    fn dispatch_error_display_carries_classification_and_message() {
        assert_eq!(
            DispatchError::Permanent("coin x not found".into()).to_string(),
            "permanent: coin x not found"
        );
        assert_eq!(
            DispatchError::Transient("network blip".into()).to_string(),
            "transient: network blip"
        );
    }

    // ── AC-SCHED-062: claim-count bound during cooldown ──────────────────────

    #[test]
    fn bound_is_ceil_div_plus_one() {
        // 60s cooldown, 1s pause → ⌈60/1⌉ + 1 = 61.
        assert_eq!(max_claims_in_window(60, 1), 61);
        // 60s cooldown, 10s pause → ⌈60/10⌉ + 1 = 7.
        assert_eq!(max_claims_in_window(60, 10), 7);
        // Non-divisible: 65s cooldown, 10s pause → ⌈65/10⌉ + 1 = 7 + 1 = 8.
        assert_eq!(max_claims_in_window(65, 10), 8);
    }

    #[test]
    fn bound_clamps_zero_pause_to_one() {
        // A zero pause must not divide-by-zero, nor imply an unbounded loop.
        assert_eq!(max_claims_in_window(30, 0), 31);
    }

    #[test]
    fn bound_zero_window_is_one_claim() {
        // No cooldown window → a single claim, no pause needed.
        assert_eq!(max_claims_in_window(0, 5), 1);
    }
}
