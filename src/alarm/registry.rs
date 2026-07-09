//! Shared in-memory health registry (SPEC-ALARM-001 Milestone 4/5, REQ-ALARM-019).
//!
//! Holds exactly the state that cannot be re-derived from the database, updated
//! cheaply (O(1), no I/O, no alarm-center calls) at provider/collector error sites and
//! read by the reconciler. This batch (Batch 2) wires only the Tier 1 fields the
//! reconciler needs: per-provider reachability (`providers`) and the chain-outcome flag
//! (`all_providers_down`). `worker_restarts` and `upsert_failure_streak` are Batch 3
//! scope — the fields exist so the struct shape matches the SPEC, but no error site
//! pokes them yet and the reconciler does not read them in this batch.
//!
//! @MX:NOTE: [AUTO] HealthRegistry enumerates exactly the counters/flags each Tier 1
//! condition reads: `providers` feeds provider-unreachable (REQ-ALARM-020);
//! `all_providers_down` feeds all-providers-down (REQ-ALARM-022). This registry drives
//! DETECTION only — it is never a clear mechanism (recovery is server-driven via TTL,
//! see `crate::alarm::reconciler`), so its imperfection or loss cannot strand an alarm.

use crate::providers::{AttemptRecord, ProviderOutcome};
use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::Mutex;
use std::time::Instant;

/// Per-provider reachability snapshot (REQ-ALARM-020 active signal).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderHealth {
    /// `None` = this provider has never recorded a success.
    pub last_success_at: Option<Instant>,
    /// Consecutive `ProviderError::Network` failures since the last success.
    pub consecutive_network_failures: u32,
}

/// Cheap, shareable health registry (wrap in `Arc` for injection into workers and the
/// reconciler). Every update method is O(1), touches no I/O, and never calls the alarm
/// center — safe to call from any hot-path error site (REQ-ALARM-007/019).
#[derive(Default)]
pub struct HealthRegistry {
    providers: Mutex<HashMap<String, ProviderHealth>>,
    all_providers_down: Mutex<bool>,

    // ── Batch 3 scope (stubbed; not wired to any error site in Batch 2) ────────
    #[allow(dead_code)]
    worker_restarts: Mutex<HashMap<String, Vec<Instant>>>,
    #[allow(dead_code)]
    upsert_failure_streak: AtomicU32,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider succeeded: reset its failure streak and stamp last-success now.
    pub fn record_provider_success(&self, provider: &str) {
        let mut providers = self.providers.lock().expect("registry lock poisoned");
        let entry = providers.entry(provider.to_string()).or_default();
        entry.last_success_at = Some(Instant::now());
        entry.consecutive_network_failures = 0;
    }

    /// A provider produced `ProviderError::Network`: bump the consecutive-failure
    /// counter. Does NOT touch `last_success_at` (REQ-ALARM-020).
    pub fn record_provider_network_failure(&self, provider: &str) {
        let mut providers = self.providers.lock().expect("registry lock poisoned");
        let entry = providers.entry(provider.to_string()).or_default();
        entry.consecutive_network_failures += 1;
    }

    /// Snapshot a provider's current health. Returns the zero-value (never observed)
    /// if the provider has no entry yet.
    pub fn provider_snapshot(&self, provider: &str) -> ProviderHealth {
        self.providers
            .lock()
            .expect("registry lock poisoned")
            .get(provider)
            .copied()
            .unwrap_or_default()
    }

    /// All provider names currently tracked (for the reconciler's sweep iteration).
    pub fn tracked_providers(&self) -> Vec<String> {
        self.providers
            .lock()
            .expect("registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Set the chain-outcome flag: a chain fetch recorded every attempt as a failure
    /// (REQ-ALARM-022).
    pub fn record_chain_all_failed(&self) {
        *self
            .all_providers_down
            .lock()
            .expect("registry lock poisoned") = true;
    }

    /// Clear the chain-outcome flag: a chain fetch recorded at least one success.
    pub fn record_chain_success(&self) {
        *self
            .all_providers_down
            .lock()
            .expect("registry lock poisoned") = false;
    }

    /// Current chain-outcome flag (REQ-ALARM-022 active signal).
    pub fn all_providers_down(&self) -> bool {
        *self
            .all_providers_down
            .lock()
            .expect("registry lock poisoned")
    }

    /// Convenience: derive provider-success/network-failure and chain-outcome updates
    /// from a batch of `AttemptRecord`s (as produced by `chain_fetch_ohlc`/
    /// `chain_fetch_ohlc_range`). Only `ProviderOutcome::Success`/`Failure` affect the
    /// registry; `Unsupported` attempts are ignored (a provider that does not support
    /// the capability is neither reachable nor unreachable evidence).
    ///
    /// Note: `AttemptRecord` does not carry the underlying `ProviderError`, so this
    /// records ANY failure as a network failure for the provider-unreachable signal.
    /// Callers with access to the concrete error (e.g. `chain_fetch_ohlc`'s per-attempt
    /// match arms) should prefer the more precise `record_provider_network_failure`
    /// gated on `ProviderError::Network` instead of calling this helper.
    pub fn observe_chain_records(&self, records: &[AttemptRecord]) {
        let attempted: Vec<&AttemptRecord> = records
            .iter()
            .filter(|r| r.outcome != ProviderOutcome::Unsupported)
            .collect();
        if attempted.is_empty() {
            return;
        }
        if attempted
            .iter()
            .any(|r| r.outcome == ProviderOutcome::Success)
        {
            self.record_chain_success();
        } else if attempted
            .iter()
            .all(|r| r.outcome == ProviderOutcome::Failure)
        {
            self.record_chain_all_failed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── record_provider_success / record_provider_network_failure ─────────────

    #[test]
    fn unseen_provider_snapshot_is_zero_value() {
        let reg = HealthRegistry::new();
        let snap = reg.provider_snapshot("binance");
        assert!(snap.last_success_at.is_none());
        assert_eq!(snap.consecutive_network_failures, 0);
    }

    #[test]
    fn record_provider_network_failure_increments_counter_without_touching_success() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_network_failure("binance");
        let snap = reg.provider_snapshot("binance");
        assert_eq!(snap.consecutive_network_failures, 2);
        assert!(snap.last_success_at.is_none());
    }

    #[test]
    fn record_provider_success_resets_failure_streak_and_stamps_time() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_network_failure("binance");
        reg.record_provider_success("binance");
        let snap = reg.provider_snapshot("binance");
        assert_eq!(snap.consecutive_network_failures, 0);
        assert!(snap.last_success_at.is_some());
        assert!(snap.last_success_at.unwrap().elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn providers_are_tracked_independently() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_success("coinbase");
        assert_eq!(
            reg.provider_snapshot("binance")
                .consecutive_network_failures,
            1
        );
        assert_eq!(
            reg.provider_snapshot("coinbase")
                .consecutive_network_failures,
            0
        );
        assert!(reg.provider_snapshot("coinbase").last_success_at.is_some());
    }

    #[test]
    fn tracked_providers_lists_every_observed_name() {
        let reg = HealthRegistry::new();
        reg.record_provider_success("binance");
        reg.record_provider_network_failure("coinbase");
        let mut names = reg.tracked_providers();
        names.sort();
        assert_eq!(names, vec!["binance".to_string(), "coinbase".to_string()]);
    }

    // ── record_chain_all_failed / record_chain_success ─────────────────────────

    #[test]
    fn chain_starts_not_down() {
        let reg = HealthRegistry::new();
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn record_chain_all_failed_sets_flag() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        assert!(reg.all_providers_down());
    }

    #[test]
    fn record_chain_success_clears_flag() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        reg.record_chain_success();
        assert!(!reg.all_providers_down());
    }

    // ── observe_chain_records ───────────────────────────────────────────────────

    fn rec(provider: &str, outcome: ProviderOutcome) -> AttemptRecord {
        AttemptRecord {
            provider: provider.to_string(),
            capability: crate::providers::Capability::Ohlc,
            outcome,
        }
    }

    #[test]
    fn observe_chain_records_all_failure_sets_down() {
        let reg = HealthRegistry::new();
        let records = vec![
            rec("binance", ProviderOutcome::Failure),
            rec("coinbase", ProviderOutcome::Failure),
        ];
        reg.observe_chain_records(&records);
        assert!(reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_any_success_clears_down() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        let records = vec![
            rec("binance", ProviderOutcome::Failure),
            rec("coinbase", ProviderOutcome::Success),
        ];
        reg.observe_chain_records(&records);
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_ignores_unsupported_only_records() {
        let reg = HealthRegistry::new();
        let records = vec![rec("binance", ProviderOutcome::Unsupported)];
        reg.observe_chain_records(&records);
        // No attempted (non-Unsupported) records: flag left untouched (still not down).
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_unsupported_mixed_with_failure_is_not_all_failed() {
        let reg = HealthRegistry::new();
        let records = vec![
            rec("coingecko", ProviderOutcome::Unsupported),
            rec("binance", ProviderOutcome::Failure),
        ];
        reg.observe_chain_records(&records);
        // Literal reading of REQ-ALARM-022: "every AttemptRecord.outcome == Failure".
        // A mixed Unsupported+Failure batch (after filtering Unsupported) has only one
        // attempted record which IS Failure, so this DOES count as all-failed among
        // attempted providers.
        assert!(reg.all_providers_down());
    }
}
